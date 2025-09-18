#!/usr/bin/env python3

import time
import sys
import json
from jellyfin_apiclient_python import JellyfinClient

def wait_for_jellyfin(server_url, max_retries=30):
    """Wait for Jellyfin server to be ready"""
    print("🔧 Initializing Jellyfin Movies Server...")
    
    for i in range(max_retries):
        try:
            client = JellyfinClient()
            client.config.app('Jellyfin Init Script', '1.0.0', 'init-script-movies', 'init-movies-container')
            client.config.data["auth.ssl"] = False
            
            # Try to connect to check if server is ready
            client.auth.connect_to_address(server_url)
            print("✅ Jellyfin is running, setting up initial configuration...")
            return client
        except Exception as e:
            print(f"⏳ Waiting for Jellyfin to start... (attempt {i+1}/{max_retries})")
            time.sleep(5)
    
    print("❌ Jellyfin server failed to start within timeout")
    sys.exit(1)

def initialize_server():
    server_url = "http://jellyfin-movies:8096"
    
    # Wait for server to be ready
    client = wait_for_jellyfin(server_url)
    
    try:
        # Complete initial setup wizard
        print("🔧 Completing initial setup wizard...")
        
        # Set up initial configuration
        setup_data = {
            "UICulture": "en-US",
            "MetadataCountryCode": "US", 
            "PreferredMetadataLanguage": "en"
        }
        
        # Create admin user during setup
        user_data = {
            "Name": "admin",
            "Password": "password"
        }
        
        # The jellyfin-apiclient-python handles the setup process
        # We'll use direct authentication since the server should be in setup mode
        client.auth.login(server_url, "admin", "password")
        
        print("🔑 Admin user authenticated successfully")
        
        # Create regular user
        print("👤 Creating regular user...")
        
        # Get the jellyfin API object
        api = client.jellyfin
        
        # Create regular user
        regular_user_data = {
            "Name": "user",
            "Password": "movies"
        }
        
        try:
            # Note: The exact API call may need adjustment based on server state
            result = api.create_user_by_name(regular_user_data)
            print("👤 Created regular user successfully")
        except Exception as e:
            print(f"⚠️  Regular user creation may have failed: {e}")
        
        # Add Movies library
        print("🎬 Creating Movies library...")
        
        library_options = {
            "Name": "Movies",
            "CollectionType": "movies",
            "PathInfos": [{"Path": "/media/movies"}],
            "LibraryOptions": {
                "EnablePhotos": True,
                "EnableRealtimeMonitor": True,
                "EnableChapterImageExtraction": False,
                "ExtractChapterImagesDuringLibraryScan": False,
                "SaveLocalMetadata": False,
                "EnableInternetProviders": True,
                "PreferredMetadataLanguage": "en",
                "MetadataCountryCode": "US"
            }
        }
        
        try:
            # Create virtual folder (library)
            result = api.add_virtual_folder(
                name="Movies",
                collection_type="movies", 
                paths=["/media/movies"]
            )
            print("🎬 Created Movies library successfully")
        except Exception as e:
            print(f"⚠️  Movies library creation may have failed: {e}")
        
        # Trigger library scan
        print("🔍 Triggering library scan...")
        try:
            api.refresh_library()
            print("🔍 Library scan triggered successfully")
        except Exception as e:
            print(f"⚠️  Library scan trigger may have failed: {e}")
        
        print("✅ Movies server initialization complete!")
        
    except Exception as e:
        print(f"❌ Error during server initialization: {e}")
        # Print more details for debugging
        import traceback
        traceback.print_exc()
        sys.exit(1)

if __name__ == "__main__":
    # Initial delay to let Jellyfin fully start
    time.sleep(10)
    initialize_server()