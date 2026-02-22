# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "httpx",
#     "jellyfin-apiclient-python",
# ]
# ///

import httpx
import os
from jellyfin_apiclient_python import JellyfinClient
import time

AUTHORIZATION_HEADER = 'MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDIuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDIuMHwxNzU4NDQ4NDAzOTk5", Version="10.10.7"'
AUTHORIZATION = {"Authorization": AUTHORIZATION_HEADER}

SERVER_URL = os.environ.get("URL", "http://localhost:8096")
ADMIN_PASSWORD = "password"
ADMIN_USER = "admin"

USERNAME = os.environ.get("USERNAME","user")
PASSWORD = os.environ.get("PASSWORD","password")

COLLECTION_NAME = os.environ.get("COLLECTION_NAME", "Movies")
COLLECTION_PATH = os.environ.get("COLLECTION_PATH","/media/movies")
COLLECTION_TYPE = os.environ.get("COLLECTION_TYPE", "movies")

def initialize_server():

    with httpx.Client(headers=AUTHORIZATION, base_url=SERVER_URL) as client:

        # Retry logic for getting system info until we get a response
        max_retries = 10
        retry_delay = 5  # seconds
        for attempt in range(max_retries):
            try:
                info = client.get("/System/Info/Public").json()
                break  # Success, exit loop
            except Exception as e:
                print(f"Attempt {attempt + 1} failed: {e}")
                if attempt < max_retries - 1:
                    time.sleep(retry_delay)
                else:
                    raise  # Re-raise after max retries

        if info and info.get("Version"):
            print(f"ℹ️  Jellyfin version: {info['Version']}")
            if info.get("StartupWizardCompleted"):
                print("ℹ️  Setup wizard already completed, skipping initialization")
                return

        default_user = client.get("/Startup/User")
        default_user.raise_for_status()
        print("✅ Retrieved default user: ", default_user.json())

        client.post("/Startup/User", json={"Name": ADMIN_USER,"Password": ADMIN_PASSWORD}).raise_for_status()
        print(f"✅ Created user '{ADMIN_USER}' with password '{ADMIN_PASSWORD}'")
        client.post("/Startup/Configuration", json={"UICulture": "en-US","MetadataCountryCode": "US","PreferredMetadataLanguage": "en"}).raise_for_status()
        print("✅ Configured server settings")
        client.post("/Startup/RemoteAccess", json={"EnableRemoteAccess": True,"EnableAutomaticPortMapping": True}).raise_for_status()
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
        print(f"Failed to create user '{USERNAME}'. It might already exist. Error: {e}")

def create_library(client: JellyfinClient):
    try:
        folders = client.jellyfin.get_media_folders()
        for folder in folders['Items']:
            if folder['Name'] == COLLECTION_NAME:
                print(f"Library '{COLLECTION_NAME}' already exists, skipping creation")
                return
            
        client.jellyfin.add_media_library(
            name=COLLECTION_NAME,
            collectionType=COLLECTION_TYPE,
            paths=[COLLECTION_PATH],
        )
        print(f"✅ Created library '{COLLECTION_NAME}'")
    except Exception as e:
        print(f"❌ Failed to create library: {e}")

    client.jellyfin.refresh_library()


if __name__ == "__main__":
    initialize_server()
    client = JellyfinClient()
    client.config.app('auto-init', '0.0.1', 'foo', 'bar')
    client.config.data["auth.ssl"] = False
    client.auth.connect_to_address(SERVER_URL)
    user = client.auth.login(SERVER_URL, username=ADMIN_USER, password=ADMIN_PASSWORD)
    print(f"✅ Authenticated as '{user['User']['Name']}'")
    create_users(client)
    create_library(client)




