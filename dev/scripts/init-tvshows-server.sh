#!/bin/bash

# Wait for Jellyfin to start
sleep 10

JELLYFIN_URL="http://jellyfin-tvshows:8096"
echo "🔧 Initializing Jellyfin TV Shows Server..."

# Check if Jellyfin is running
until curl -s "$JELLYFIN_URL/health" > /dev/null; do
    echo "⏳ Waiting for Jellyfin to start..."
    sleep 5
done

echo "✅ Jellyfin is running, setting up initial configuration..."

# Complete initial setup (skip wizard)
curl -X POST "$JELLYFIN_URL/Startup/Configuration" \
  -H "Content-Type: application/json" \
  -d '{
    "UICulture": "en-US",
    "MetadataCountryCode": "US",
    "PreferredMetadataLanguage": "en"
  }' > /dev/null 2>&1

sleep 2

# Create admin user
curl -X POST "$JELLYFIN_URL/Startup/User" \
  -H "Content-Type: application/json" \
  -d '{
    "Name": "admin",
    "Password": "password"
  }' > /dev/null 2>&1

sleep 2

# Complete startup
curl -X POST "$JELLYFIN_URL/Startup/Complete" > /dev/null 2>&1

sleep 5

# Get auth token by logging in as admin
AUTH_RESPONSE=$(curl -s -X POST "$JELLYFIN_URL/Users/AuthenticateByName" \
  -H "Content-Type: application/json" \
  -d '{
    "Username": "admin",
    "Pw": "password",
    "App": "Jellyfin Init Script",
    "AppVersion": "1.0.0",
    "DeviceId": "init-script-tvshows",
    "DeviceName": "Init Script"
  }')

TOKEN=$(echo "$AUTH_RESPONSE" | grep -o '"AccessToken":"[^"]*"' | cut -d'"' -f4)

if [ -z "$TOKEN" ]; then
    echo "❌ Failed to get auth token"
    echo "Auth response: $AUTH_RESPONSE"
    exit 1
fi

echo "🔑 Got authentication token"

# Create regular user
curl -s -X POST "$JELLYFIN_URL/Users/New" \
  -H "Content-Type: application/json" \
  -H "Authorization: MediaBrowser Token=\"$TOKEN\"" \
  -d '{
    "Name": "user",
    "Password": "shows"
  }' > /dev/null

echo "👤 Created users"

# Add TV shows library
curl -s -X POST "$JELLYFIN_URL/Library/VirtualFolders?collectionType=tvshows&refreshLibrary=true&name=TV%20Shows" \
  -H "Content-Type: application/json" \
  -H "Authorization: MediaBrowser Token=\"$TOKEN\"" \
  -d '{
    "LibraryOptions": {
      "EnablePhotos": true,
      "EnableRealtimeMonitor": true,
      "EnableChapterImageExtraction": false,
      "ExtractChapterImagesDuringLibraryScan": false,
      "PathInfos": [
        {
          "Path": "/media/tv-shows",
          "NetworkPath": ""
        }
      ],
      "SaveLocalMetadata": false,
      "EnableInternetProviders": true,
      "EnableAutomaticSeriesGrouping": true,
      "PreferredMetadataLanguage": "en",
      "MetadataCountryCode": "US"
    }
  }' > /dev/null

echo "📺 Created TV Shows library"

# Trigger library scan
curl -s -X POST "$JELLYFIN_URL/Library/Refresh" \
  -H "Authorization: MediaBrowser Token=\"$TOKEN\"" > /dev/null

echo "🔍 Triggered library scan"
echo "✅ TV Shows server initialization complete!"