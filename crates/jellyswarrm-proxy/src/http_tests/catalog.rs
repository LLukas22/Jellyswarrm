//! Read contracts derived from useFetchItems, itemdetails, itemsByName and the
//! legacy ApiClient. Each case pins the backend, credentials, query and DTO shape.
use super::*;
use wiremock::{
    matchers::{method, path, query_param},
    Mock, ResponseTemplate,
};

#[derive(Clone, Copy)]
pub(super) enum Shape {
    List,
    Bare,
    Detail,
    Segments,
    Theme,
    Filters,
    Lyrics,
    Counts,
    Playlist,
    PlaylistUser,
    Recommendations,
    ImageInfo,
    Fonts,
}

pub(super) const METHOD: &str = "GET";
macro_rules! contracts {
    ($($name:ident: $route:literal, $shape:ident;)+) => {
        pub(super) const ROUTES: &[&str] = &[$($route),+];
        $(contract_test!($name, read_contract, $route, Shape::$shape);)+
    };
}

contracts! {
    modern_items: "/Items?ParentId={item}&UserId={user}&Recursive=true&SortBy=SortName&Limit=20", List;
    legacy_items: "/Users/{user}/Items?ParentId={item}&Recursive=true&Limit=20", List;
    modern_detail: "/Items/{item}?UserId={user}&Fields=MediaSources,Chapters", Detail;
    legacy_detail: "/Users/{user}/Items/{item}", Detail;
    root_folder: "/Users/{user}/Items/Root", Detail;
    modern_latest: "/Items/Latest?ParentId={item}&UserId={user}&Limit=8", Bare;
    legacy_latest: "/Users/{user}/Items/Latest?ParentId={item}&Limit=8", Bare;
    modern_resume: "/UserItems/Resume?ParentId={item}&UserId={user}&Limit=8", List;
    legacy_resume: "/Users/{user}/Items/Resume?ParentId={item}&Limit=8", List;
    suggestions: "/Items/Suggestions?ParentId={item}&UserId={user}&Limit=8", List;
    recommendations: "/Movies/Recommendations?UserId={user}&ParentId={item}&ItemLimit=8", Recommendations;
    similar: "/Items/{item}/Similar?UserId={user}&Limit=12", List;
    ancestors: "/Items/{item}/Ancestors?UserId={user}", Bare;
    intros: "/Users/{user}/Items/{item}/Intros", List;
    local_trailers: "/Users/{user}/Items/{item}/LocalTrailers", Bare;
    additional_parts_returns_client_ids: "/Videos/{item}/AdditionalParts?UserId={user}", List;
    special_features: "/Users/{user}/Items/{item}/SpecialFeatures", Bare;
    theme_media: "/Items/{item}/ThemeMedia?UserId={user}&InheritFromParent=true", Theme;
    instant_mix: "/Items/{item}/InstantMix?UserId={user}&Limit=50", List;
    item_collections: "/Items/{item}/Collections?UserId={user}", List;
    seasons: "/Shows/{item}/Seasons?UserId={user}&Fields=MediaSources", List;
    episodes: "/Shows/{item}/Episodes?UserId={user}&StartIndex=0&Limit=50", List;
    next_up: "/Shows/NextUp?SeriesId={item}&UserId={user}&Limit=1", List;
    upcoming: "/Shows/Upcoming?ParentId={item}&UserId={user}", List;
    genres: "/Genres?ParentId={item}&UserId={user}&IncludeItemTypes=Movie", List;
    named_genre: "/Genres/Drama%20%26%20Mystery?UserId={user}", Detail;
    named_music_genre: "/MusicGenres/Rock?UserId={user}", Detail;
    named_artist: "/Artists/Artist?UserId={user}", Detail;
    studios: "/Studios?ParentId={item}&UserId={user}&IncludeItemTypes=Movie", List;
    people: "/Persons?ParentId={item}&UserId={user}&SearchTerm=Actor", List;
    artists: "/Artists?ParentId={item}&UserId={user}&IncludeItemTypes=Audio", List;
    album_artists: "/Artists/AlbumArtists?ParentId={item}&UserId={user}", List;
    filters: "/Items/Filters?ParentId={item}&UserId={user}&IncludeItemTypes=Movie", Filters;
    filters2: "/Items/Filters2?ParentId={item}&UserId={user}&IncludeItemTypes=Movie", Filters;
    playlist_items: "/Playlists/{item}/Items?UserId={user}&StartIndex=0&Limit=100", List;
    playlist_detail: "/Playlists/{item}?UserId={user}", Playlist;
    playlist_user: "/Playlists/{item}/Users/{user}", PlaylistUser;
    media_segments: "/MediaSegments/{item}?includeSegmentTypes=Intro,Outro", Segments;
    lyrics: "/Audio/{item}/Lyrics", Lyrics;
    image_info: "/Items/{item}/Images", ImageInfo;
    fallback_fonts: "/FallbackFont/Fonts", Fonts;
    channels: "/LiveTv/Channels?UserId={user}&ParentId={item}", List;
    channel: "/LiveTv/Channels/{item}?UserId={user}", Detail;
    programs: "/LiveTv/Programs?ChannelIds={item}&UserId={user}&EnableImages=true", List;
    program: "/LiveTv/Programs/{item}?UserId={user}", Detail;
    recommended_programs: "/LiveTv/Programs/Recommended?ParentId={item}&UserId={user}", List;
    recordings: "/LiveTv/Recordings?ParentId={item}&UserId={user}", List;
    recording: "/LiveTv/Recordings/{item}?UserId={user}", Detail;
    recording_folders: "/LiveTv/Recordings/Folders?ParentId={item}&UserId={user}", List;
    timers: "/LiveTv/Timers", List;
    timer: "/LiveTv/Timers/{item}", Detail;
    recording_defaults_follow_program_owner: "/LiveTv/Timers/Defaults?ProgramId={item}", Detail;
    series_timers: "/LiveTv/SeriesTimers?SortBy=SortName", List;
    series_timer: "/LiveTv/SeriesTimers/{item}", Detail;
    item_counts: "/Items/Counts?UserId={user}", Counts;
}

async fn read_contract(route: &str, shape: Shape) {
    let f = Fixture::for_scenario(Scenario::NormalServers).await;
    f.healthy().await;
    let owner = usize::from(route.contains("{item}"));
    let original = f.original(owner).await;
    let item = json!({"Id":original,"Name":"Contract movie","Type":"Movie","MediaSources":[{"Id":original,"Name":"1080p","MediaStreams":[]}],"UserData":{"ItemId":original,"Played":false,"PlaybackPositionTicks":123,"PlayCount":1,"IsFavorite":false,"Key":"key"}});
    let upstream_body = match shape {
        Shape::List => json!({"Items":[item],"TotalRecordCount":1,"StartIndex":0}),
        Shape::Bare => json!([item]),
        Shape::Detail => item,
        Shape::Segments => {
            json!({"Items":[{"Id":"11111111111111111111111111111111","ItemId":original,"Type":"Intro","StartTicks":100,"EndTicks":200}],"TotalRecordCount":1})
        }
        Shape::Theme => {
            json!({"ThemeSongsResult":{"Items":[item.clone()],"TotalRecordCount":1},"ThemeVideosResult":{"Items":[item],"TotalRecordCount":1}})
        }
        Shape::Filters => {
            json!({"Genres":["Drama"],"Tags":["tag"],"Years":[2026],"OfficialRatings":["PG"]})
        }
        Shape::Lyrics => {
            json!({"Metadata":{"Artist":"Artist","Title":"Title"},"Lyrics":[{"Text":"First line","Start":123}]})
        }
        Shape::Counts => json!({"MovieCount":12,"EpisodeCount":24}),
        Shape::Playlist => {
            json!({"OpenAccess":false,"Shares":[{"UserId":format!("backend-user-{owner}"),"CanEdit":true}],"ItemIds":[original]})
        }
        Shape::PlaylistUser => json!({"UserId":format!("backend-user-{owner}"),"CanEdit":true}),
        Shape::Recommendations => {
            json!([{"RecommendationType":"SimilarToRecentlyPlayed","Items":[item]}])
        }
        Shape::ImageInfo => {
            json!([{"ImageType":"Primary","ImageIndex":0,"ImageTag":original,"Width":320,"Height":180}])
        }
        Shape::Fonts => json!([{"Name":"Noto.ttf","Size":1024}]),
    };
    let client_path = route
        .replace("{item}", &f.items[owner])
        .replace("{user}", &f.user.id);
    let upstream_path = route
        .replace("{item}", &original)
        .replace("{user}", &format!("backend-user-{owner}"));
    let upstream_url = url::Url::parse(&format!("http://backend{upstream_path}")).unwrap();
    let mut mock = Mock::given(method("GET")).and(path(upstream_url.path()));
    for (key, value) in upstream_url.query_pairs() {
        mock = mock.and(query_param(key.as_ref(), value.as_ref()));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .expect(1)
        .mount(&f.upstreams[owner])
        .await;
    let response = f
        .request(reqwest::Method::GET, &client_path, &f.user, "tv")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{route}: {text}");
    let result: Value = serde_json::from_str(&text).unwrap();
    match shape {
        Shape::List => {
            assert_eq!(result["Items"][0]["Id"], f.items[owner]);
            assert_eq!(result["TotalRecordCount"], 1);
        }
        Shape::Bare => assert_eq!(result[0]["Id"], f.items[owner]),
        Shape::Detail => assert_eq!(result["Id"], f.items[owner]),
        Shape::Segments => {
            assert_eq!(result["Items"][0]["ItemId"], f.items[owner]);
            assert_eq!(result["Items"][0]["EndTicks"], 200);
        }
        Shape::Theme => assert_eq!(result["ThemeSongsResult"]["Items"][0]["Id"], f.items[owner]),
        Shape::Filters | Shape::Lyrics | Shape::Counts => assert_eq!(result, upstream_body),
        Shape::Playlist => {
            assert_eq!(result["Shares"][0]["UserId"], f.user.id.as_str());
            assert_eq!(result["ItemIds"][0], f.items[owner]);
        }
        Shape::PlaylistUser => assert_eq!(result["UserId"], f.user.id.as_str()),
        Shape::Recommendations => assert_eq!(result[0]["Items"][0]["Id"], f.items[owner]),
        Shape::ImageInfo => {
            assert_eq!(result[0]["Width"], 320);
            assert_eq!(result[0]["ImageTag"], f.items[owner]);
        }
        Shape::Fonts => assert_eq!(result, upstream_body),
    }
    assert!(
        f.upstreams[1 - owner]
            .received_requests()
            .await
            .unwrap()
            .is_empty(),
        "wrong backend for {route}"
    );
    let requests = f.upstreams[owner].received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{route}");
    assert!(requests[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .contains(&format!("backend-token-{owner}")));
}
