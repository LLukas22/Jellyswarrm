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

AUTHORIZATION_HEADER = 'MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDIuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDIuMHwxNzU4NDQ4NDAzOTk5", Version="10.10.7"'
AUTHORIZATION = {"Authorization": AUTHORIZATION_HEADER}

SERVER_URL = os.environ.get("URL", "http://localhost:8096")
SERVER_NAME = os.environ.get("SERVER_NAME", "Jellyfin Dev")
ADMIN_PASSWORD = "password"
ADMIN_USER = "admin"

USERNAME = os.environ.get("USERNAME", "user")
PASSWORD = os.environ.get("PASSWORD", "password")

COLLECTION_NAME = os.environ.get("COLLECTION_NAME", "Movies")
COLLECTION_PATH = os.environ.get("COLLECTION_PATH", "/media/movies")
COLLECTION_TYPE = os.environ.get("COLLECTION_TYPE", "movies")


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


def create_users(client: JellyfinClient):
    try:
        users = client.jellyfin.get_users()
        for user in users:
            if user['Name'] == USERNAME:
                print(f"User '{USERNAME}' already exists, skipping creation")
                return
        client.jellyfin.new_user(name=USERNAME, pw=PASSWORD)
        print(f"✅ Created user '{USERNAME}' with password '{PASSWORD}'")
    except Exception as e:
        print(f"Failed to create user '{USERNAME}': {e}")
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
        folders = client.jellyfin.get_media_folders()
        library_exists = any(
            folder["Name"] == COLLECTION_NAME for folder in folders["Items"]
        )
        if library_exists:
            print(f"Library '{COLLECTION_NAME}' already exists, refreshing it")
        else:
            client.jellyfin.add_media_library(
                name=COLLECTION_NAME,
                collectionType=COLLECTION_TYPE,
                paths=[COLLECTION_PATH],
            )
            print(f"✅ Created library '{COLLECTION_NAME}'")
    except Exception as e:
        print(f"❌ Failed to create library: {e}")
        raise

    client.jellyfin.refresh_library()


if __name__ == "__main__":
    initialize_server()
    client = JellyfinClient()
    client.config.app("auto-init", "0.0.1", "foo", "bar")
    client.config.data["auth.ssl"] = False
    client.auth.connect_to_address(SERVER_URL)
    user = client.auth.login(SERVER_URL, username=ADMIN_USER, password=ADMIN_PASSWORD)
    print(f"✅ Authenticated as '{user['User']['Name']}'")
    set_server_name(client)
    create_users(client)
    create_library(client)
