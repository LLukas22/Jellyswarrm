//! Request-independent library discovery. Complete administrator inventories may
//! replace stored inventories; mapped-user sightings are additive, never pruning.
use crate::{
    auth::{
        encryption::{decrypt_password, HashedPassword},
        mapping_authentication::MappingAuthenticator,
        service::User,
        UserAuthorizationService,
    },
    libraries::{
        service::{normalize_library_id, DiscoveredLibrary},
        VirtualLibraryService,
    },
    servers::{Server, ServerStorageService},
};

#[derive(Debug)]
pub(crate) enum DiscoveryStage {
    Servers,
    Users,
    AdminCredentials,
    Mapping,
    Authentication,
    Inventory,
    Persistence,
}

#[derive(Debug)]
pub(crate) struct DiscoveryIssue {
    pub server: Option<String>,
    pub user: Option<String>,
    pub stage: DiscoveryStage,
}

pub(crate) struct LibraryAdministration<'a> {
    servers: &'a ServerStorageService,
    users: &'a UserAuthorizationService,
    libraries: &'a VirtualLibraryService,
    authenticator: MappingAuthenticator<'a>,
    admin_key: HashedPassword,
    transport: reqwest::Client,
}

impl<'a> LibraryAdministration<'a> {
    pub(crate) fn new(
        servers: &'a ServerStorageService,
        users: &'a UserAuthorizationService,
        libraries: &'a VirtualLibraryService,
        authenticator: MappingAuthenticator<'a>,
        admin_key: HashedPassword,
        transport: reqwest::Client,
    ) -> Self {
        Self {
            servers,
            users,
            libraries,
            authenticator,
            admin_key,
            transport,
        }
    }

    pub(crate) async fn refresh(&self) -> Vec<DiscoveryIssue> {
        let mut issues = Vec::new();
        let servers = match self.servers.list_servers().await {
            Ok(servers) => servers,
            Err(error) => {
                tracing::error!(%error, "Could not list servers for library discovery");
                return vec![DiscoveryIssue {
                    server: None,
                    user: None,
                    stage: DiscoveryStage::Servers,
                }];
            }
        };
        let users = match self.users.list_users().await {
            Ok(users) => users,
            Err(error) => {
                tracing::error!(%error, "Could not list mapped users for library discovery");
                issues.push(DiscoveryIssue {
                    server: None,
                    user: None,
                    stage: DiscoveryStage::Users,
                });
                Vec::new()
            }
        };
        for server in servers {
            let admin = match self.servers.get_server_admin(server.id).await {
                Ok(admin) => admin,
                Err(error) => {
                    tracing::error!(%error, server_id = %server.id, "Could not load administrator credentials");
                    issue(&mut issues, &server, None, DiscoveryStage::AdminCredentials);
                    None
                }
            };
            let Some(admin) = admin else {
                self.discover_mapped(&server, &users, &mut issues).await;
                continue;
            };
            let password = match decrypt_password(&admin.password, &self.admin_key) {
                Ok(password) => password,
                Err(error) => {
                    tracing::error!(%error, server_id = %server.id, "Could not decrypt administrator credentials");
                    issue(&mut issues, &server, None, DiscoveryStage::AdminCredentials);
                    continue;
                }
            };
            let client = match jellyfin_api::JellyfinClient::new_with_client(
                server.url.as_str(),
                crate::config::CLIENT_INFO.clone(),
                self.transport.clone(),
            ) {
                Ok(client) => client,
                Err(_) => {
                    issue(&mut issues, &server, None, DiscoveryStage::Authentication);
                    continue;
                }
            };
            if client
                .authenticate_by_name(&admin.username, password.as_str())
                .await
                .is_err()
            {
                issue(&mut issues, &server, None, DiscoveryStage::Authentication);
                continue;
            }
            let folders = client.get_media_folders(None).await;
            // This operation owns its temporary password-authenticated session.
            // Explicit logout never applies to durable mapped QC credentials.
            let _ = client.logout().await;
            let folders = match folders {
                Ok(folders) => folders,
                Err(_) => {
                    issue(&mut issues, &server, None, DiscoveryStage::Inventory);
                    continue;
                }
            };
            let libraries = folders
                .into_iter()
                .filter_map(|folder| discovered(server.id, folder))
                .collect::<Vec<_>>();
            if let Err(error) = self
                .libraries
                .replace_discovered_libraries(server.id, &libraries)
                .await
            {
                tracing::error!(%error, server_id = %server.id, "Could not store complete library inventory");
                issue(&mut issues, &server, None, DiscoveryStage::Persistence);
            }
        }
        issues
    }

    async fn discover_mapped(
        &self,
        server: &Server,
        users: &[User],
        issues: &mut Vec<DiscoveryIssue>,
    ) {
        let mut libraries = Vec::new();
        for user in users {
            let mapping = match self.users.get_server_mapping(&user.id, server).await {
                Ok(Some(mapping)) => mapping,
                Ok(None) => continue,
                Err(_) => {
                    issue(issues, server, Some(user), DiscoveryStage::Mapping);
                    continue;
                }
            };
            let authenticated = match self
                .authenticator
                .authenticate(user, server, &mapping)
                .await
            {
                Ok(authenticated) => authenticated,
                Err(_) => {
                    issue(issues, server, Some(user), DiscoveryStage::Authentication);
                    continue;
                }
            };
            let folders = authenticated
                .client
                .get_media_folders(Some(&authenticated.user.id))
                .await;
            authenticated.close().await;
            match folders {
                Ok(folders) => libraries.extend(
                    folders
                        .into_iter()
                        .filter_map(|folder| discovered(server.id, folder)),
                ),
                Err(_) => issue(issues, server, Some(user), DiscoveryStage::Inventory),
            }
        }
        if let Err(error) = self.libraries.track_discovered_libraries(&libraries).await {
            tracing::error!(%error, server_id = %server.id, "Could not store mapped library sightings");
            issue(issues, server, None, DiscoveryStage::Persistence);
        }
    }
}

fn issue(
    issues: &mut Vec<DiscoveryIssue>,
    server: &Server,
    user: Option<&User>,
    stage: DiscoveryStage,
) {
    issues.push(DiscoveryIssue {
        server: Some(server.name.clone()),
        user: user.map(|user| user.original_username.clone()),
        stage,
    });
}

fn discovered(
    server_id: crate::servers::id::ServerId,
    folder: jellyfin_api::models::MediaFolder,
) -> Option<DiscoveredLibrary> {
    if folder
        .collection_type
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("livetv"))
    {
        return None;
    }
    Some(DiscoveredLibrary {
        server_id,
        original_library_id: normalize_library_id(&folder.id),
        name: folder.name,
        collection_type: folder.collection_type.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::encryption::{encrypt_password, Password},
        config::{MediaStreamingMode, MIGRATOR},
        media::MediaStorageService,
    };
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn additive_or_failed_discovery_preserves_inventory_but_complete_admin_inventory_can_prune(
    ) {
        let upstream = MockServer::start().await;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let registry = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool.clone());
        let libraries = VirtualLibraryService::new(pool.clone(), registry.clone(), media);
        let users = UserAuthorizationService::new(pool);
        let id = registry
            .add_server("Backend", &upstream.uri(), 100, MediaStreamingMode::Proxy)
            .await
            .unwrap();
        libraries
            .track_discovered_libraries(&[DiscoveredLibrary {
                server_id: id,
                original_library_id: "existing".into(),
                name: "Movies".into(),
                collection_type: "movies".into(),
            }])
            .await
            .unwrap();
        let password = Password::from("admin-key");
        let key: HashedPassword = (&password).into();
        let operation = LibraryAdministration::new(
            &registry,
            &users,
            &libraries,
            MappingAuthenticator::new(&users, reqwest::Client::new(), password),
            key.clone(),
            reqwest::Client::new(),
        );
        assert!(operation.refresh().await.is_empty());
        assert_eq!(
            libraries.list_discovered_libraries().await.unwrap().len(),
            1
        );
        registry
            .add_server_admin(
                id,
                "admin",
                &encrypt_password(&"password".into(), &key).unwrap(),
            )
            .await
            .unwrap();
        Mock::given(method("POST")).and(path("/Users/AuthenticateByName"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"AccessToken":"admin-token", "User":{"Id":"admin", "Name":"Admin"}})))
            .mount(&upstream).await;
        // Unmatched inventory fetch fails: not evidence for deleting old rows.
        let problems = operation.refresh().await;
        assert!(problems
            .iter()
            .any(|problem| matches!(problem.stage, DiscoveryStage::Inventory)));
        assert_eq!(
            libraries.list_discovered_libraries().await.unwrap().len(),
            1
        );
        Mock::given(method("GET"))
            .and(path("/Library/MediaFolders"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"Items":[]})))
            .mount(&upstream)
            .await;
        assert!(operation.refresh().await.is_empty());
        assert!(libraries
            .list_discovered_libraries()
            .await
            .unwrap()
            .is_empty());
    }
}
