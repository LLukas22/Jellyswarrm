# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "httpx",
#     "jellyfin-apiclient-python",
# ]
# ///

import os
import time

import httpx
from jellyfin_apiclient_python import JellyfinClient

AUTHORIZATION_HEADER = (
    'MediaBrowser Client="Jellyswarrm Dev Initializer", Device="Docker", '
    'DeviceId="jellyswarrm-dev-init", Version="1.0.0"'
)
AUTHORIZATION = {"Authorization": AUTHORIZATION_HEADER}

SERVER_URL = os.environ.get("URL", "http://localhost:8096")
SERVER_NAME = os.environ.get("SERVER_NAME", "Jellyfin Dev")
ADMIN_PASSWORD = "password"
ADMIN_USER = "admin"

JELLYSWARRM_USERNAME = os.environ.get("JELLYSWARRM_USERNAME", "test")
JELLYSWARRM_PASSWORD = os.environ.get("JELLYSWARRM_PASSWORD", "test")

COLLECTION_NAME = os.environ.get("COLLECTION_NAME", "Movies")
COLLECTION_PATH = os.environ.get("COLLECTION_PATH", "/media/movies")
COLLECTION_TYPE = os.environ.get("COLLECTION_TYPE", "movies")
LIBRARY_COUNT = int(os.environ.get("LIBRARY_COUNT", "1"))
if LIBRARY_COUNT < 1:
    raise ValueError("LIBRARY_COUNT must be at least 1")
PLAYLIST_NAME = os.environ.get("PLAYLIST_NAME", "")
PLAYLIST_TRACK_COUNT = int(os.environ.get("PLAYLIST_TRACK_COUNT", "1"))


def wait_for_startup_user(client: httpx.Client) -> httpx.Response | None:
    max_retries = 30
    retry_delay = 2

    for attempt in range(max_retries):
        try:
            info_response = client.get("/System/Info/Public")
            info_response.raise_for_status()
            info = info_response.json()

            if info.get("StartupWizardCompleted"):
                print(f"ℹ️  Jellyfin version: {info['Version']}")
                print("ℹ️  Setup wizard already completed, skipping initialization")
                return None

            startup_user = client.get("/Startup/User")
            startup_user.raise_for_status()
            print(f"ℹ️  Jellyfin version: {info['Version']}")
            return startup_user
        except (httpx.HTTPError, ValueError) as error:
            print(f"Waiting for Jellyfin ({attempt + 1}/{max_retries}): {error}")
            if attempt < max_retries - 1:
                time.sleep(retry_delay)

    raise RuntimeError("Jellyfin did not become ready")


def initialize_server():
    with httpx.Client(headers=AUTHORIZATION, base_url=SERVER_URL) as client:
        default_user = wait_for_startup_user(client)
        if default_user is None:
            return

        print("✅ Retrieved default user: ", default_user.json())

        client.post(
            "/Startup/User",
            json={"Name": ADMIN_USER, "Password": ADMIN_PASSWORD},
        ).raise_for_status()
        print(f"✅ Created user '{ADMIN_USER}' with password '{ADMIN_PASSWORD}'")
        client.post(
            "/Startup/Configuration",
            json={
                "ServerName": SERVER_NAME,
                "UICulture": "en-US",
                "MetadataCountryCode": "US",
                "PreferredMetadataLanguage": "en",
            },
        ).raise_for_status()
        print("✅ Configured server settings")
        client.post(
            "/Startup/RemoteAccess",
            json={"EnableRemoteAccess": True, "EnableAutomaticPortMapping": True},
        ).raise_for_status()
        print("✅ Enabled remote access and automatic port mapping")
        client.post("/Startup/Complete").raise_for_status()
        print("✅ Completed setup wizard")


def configure_jellyswarrm_user(client: JellyfinClient):
    try:
        users = client.jellyfin.get_users()
        user = next(
            (user for user in users if user["Name"] == JELLYSWARRM_USERNAME),
            None,
        )
        if user is None:
            client.jellyfin.new_user(
                name=JELLYSWARRM_USERNAME,
                pw=JELLYSWARRM_PASSWORD,
            )
            user = next(
                user
                for user in client.jellyfin.get_users()
                if user["Name"] == JELLYSWARRM_USERNAME
            )
            print(f"✅ Created Jellyswarrm user '{JELLYSWARRM_USERNAME}'")

        client.jellyfin._post(
            "Users/Password",
            params={"userId": user["Id"]},
            json={"NewPw": JELLYSWARRM_PASSWORD, "ResetPassword": False},
        )

        policy = user["Policy"]
        policy.update(
            {
                "IsDisabled": False,
                "EnableRemoteAccess": True,
                "EnableMediaPlayback": True,
                "EnableAllFolders": True,
                "EnabledFolders": [],
            }
        )
        client.jellyfin._post(f"Users/{user['Id']}/Policy", json=policy)
        print(
            f"✅ Configured Jellyswarrm user '{JELLYSWARRM_USERNAME}' "
            "with access to all libraries"
        )
    except Exception as e:
        print(f"Failed to configure user '{JELLYSWARRM_USERNAME}': {e}")
        raise


def set_server_name(client: JellyfinClient):
    config = client.jellyfin.get_system_info()
    if config.get("ServerName") == SERVER_NAME:
        return

    config["ServerName"] = SERVER_NAME
    client.jellyfin._post("System/Configuration", json=config)
    print(f"✅ Set server name to '{SERVER_NAME}'")


def create_library(client: JellyfinClient):
    try:
        # VirtualFolders returns the complete configuration, not a paged Items
        # response. Keep seeding idempotent even beyond the pagination boundary.
        folders = client.jellyfin._get("Library/VirtualFolders")
        existing_names = {folder["Name"] for folder in folders}
        libraries = [(COLLECTION_NAME, [COLLECTION_PATH])]
        libraries.extend(
            (f"{SERVER_NAME} Pagination {index:02d}", [])
            for index in range(1, LIBRARY_COUNT)
        )
        for name, paths in libraries:
            if name in existing_names:
                print(f"Library '{name}' already exists, leaving it untouched")
                continue
            # Empty libraries still appear in user views. They exercise library
            # paging without duplicating media or changing playback fixtures.
            client.jellyfin.add_media_library(
                name=name,
                collectionType=COLLECTION_TYPE,
                paths=paths,
            )
            print(f"✅ Created library '{name}'")
    except Exception as e:
        print(f"❌ Failed to create library: {e}")
        raise

    client.jellyfin.refresh_library()


def seed_playlist():
    if not PLAYLIST_NAME:
        return

    # Create as the regular user so the playlist is owned by the account used
    # in Jellyswarrm, rather than by the initializer's administrator account.
    with httpx.Client(headers=AUTHORIZATION, base_url=SERVER_URL) as client:
        response = client.post(
            "/Users/AuthenticateByName",
            json={"Username": JELLYSWARRM_USERNAME, "Pw": JELLYSWARRM_PASSWORD},
        )
        response.raise_for_status()
        authentication = response.json()
        user_id = authentication["User"]["Id"]
        client.headers["Authorization"] = (
            f'{AUTHORIZATION_HEADER}, Token="{authentication["AccessToken"]}"'
        )

        response = client.get(
            "/Items",
            params={"UserId": user_id, "Recursive": True, "IncludeItemTypes": "Playlist"},
        )
        response.raise_for_status()
        if any(item["Name"] == PLAYLIST_NAME for item in response.json()["Items"]):
            print(f"ℹ️  Playlist '{PLAYLIST_NAME}' already exists, leaving it untouched")
            return

        deadline = time.monotonic() + 180
        while True:
            response = client.get(
                "/Items",
                params={
                    "UserId": user_id,
                    "Recursive": True,
                    "IncludeItemTypes": "Audio",
                    "SortBy": "SortName",
                    "SortOrder": "Ascending",
                },
            )
            response.raise_for_status()
            tracks = response.json()["Items"]
            if len(tracks) >= PLAYLIST_TRACK_COUNT:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError(
                    f"Timed out waiting for {PLAYLIST_TRACK_COUNT} tracks for '{PLAYLIST_NAME}'"
                )
            print(f"Waiting for music scan ({len(tracks)}/{PLAYLIST_TRACK_COUNT} tracks)")
            time.sleep(2)

        response = client.post(
            "/Playlists",
            json={
                "Name": PLAYLIST_NAME,
                "Ids": [track["Id"] for track in tracks],
                "UserId": user_id,
                "MediaType": "Audio",
                "IsPublic": False,
            },
        )
        response.raise_for_status()
        print(f"✅ Created playlist '{PLAYLIST_NAME}' with {len(tracks)} tracks")


if __name__ == "__main__":
    initialize_server()
    client = JellyfinClient()
    client.config.app("auto-init", "0.0.1", "foo", "bar")
    client.config.data["auth.ssl"] = False
    client.auth.connect_to_address(SERVER_URL)
    user = client.auth.login(SERVER_URL, username=ADMIN_USER, password=ADMIN_PASSWORD)
    print(f"✅ Authenticated as '{user['User']['Name']}'")
    set_server_name(client)
    configure_jellyswarrm_user(client)
    create_library(client)
    seed_playlist()
