#!/bin/bash

set -e

echo "🎬 Starting content download for Jellyfin development servers..."

# Create directory structure
echo "📁 Creating directory structure..."
mkdir -p /downloads/movies
mkdir -p /downloads/tv-shows

echo "🎭 Downloading public domain movies from Internet Archive..."

# Night of the Living Dead (1968) - Public Domain - Updated URL
echo "  📥 Night of the Living Dead (1968)..."
mkdir -p "/downloads/movies/Night of the Living Dead (1968)"
if [ ! -f "/downloads/movies/Night of the Living Dead (1968)/Night of the Living Dead (1968).mp4" ]; then
  wget --progress=bar:force -O "/downloads/movies/Night of the Living Dead (1968)/Night of the Living Dead (1968).mp4" \
    "https://archive.org/download/night_of_the_living_dead_dvd/Night.mp4" || echo "    ⚠️  Failed to download Night of the Living Dead"
else
  echo "    ✅ Night of the Living Dead already exists, skipping download"
fi

# Plan 9 from Outer Space (1959) - Public Domain - Updated URL
echo "  📥 Plan 9 from Outer Space (1959)..."
mkdir -p "/downloads/movies/Plan 9 from Outer Space (1959)"
if [ ! -f "/downloads/movies/Plan 9 from Outer Space (1959)/Plan 9 from Outer Space (1959).mp4" ]; then
  wget --progress=bar:force -O "/downloads/movies/Plan 9 from Outer Space (1959)/Plan 9 from Outer Space (1959).mp4" \
    "https://archive.org/download/plan-9-from-outer-space-1959_ed-wood/PLAN%209%20FROM%20OUTER%20SPACE%201959.ia.mp4" || echo "    ⚠️  Failed to download Plan 9 from Outer Space"
else
  echo "    ✅ Plan 9 from Outer Space already exists, skipping download"
fi

echo "🎨 Downloading Creative Commons content..."

# Big Buck Bunny
echo "  📥 Big Buck Bunny..."
mkdir -p "/downloads/movies/Big Buck Bunny (2008)"
if [ ! -f "/downloads/movies/Big Buck Bunny (2008)/Big Buck Bunny (2008).mp4" ]; then
  wget --progress=bar:force -O "/downloads/movies/Big Buck Bunny (2008)/Big Buck Bunny (2008).mp4" \
    "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4" || echo "    ⚠️  Failed to download Big Buck Bunny"
else
  echo "    ✅ Big Buck Bunny already exists, skipping download"
fi

echo "📺 Downloading public domain TV series..."

# The Cisco Kid - Public Domain Western Series
echo "  📥 The Cisco Kid (1950-1956)..."
mkdir -p "/downloads/tv-shows/The Cisco Kid (1950)/Season 01"
if [ ! -f "/downloads/tv-shows/The Cisco Kid (1950)/Season 01/The Cisco Kid - S01E01 - The Gay Caballero.mp4" ]; then
  wget --progress=bar:force -O "/downloads/tv-shows/The Cisco Kid (1950)/Season 01/The Cisco Kid - S01E01 - The Gay Caballero.mp4" \
    "https://archive.org/download/CiscoKid_201611/The%20Cisco%20Kid%20-%20The%20Gay%20Caballero.mp4" || echo "    ⚠️  Failed to download Cisco Kid S01E01"
else
  echo "    ✅ Cisco Kid S01E01 already exists, skipping download"
fi

if [ ! -f "/downloads/tv-shows/The Cisco Kid (1950)/Season 01/The Cisco Kid - S01E02 - Boomerang.mp4" ]; then
  wget --progress=bar:force -O "/downloads/tv-shows/The Cisco Kid (1950)/Season 01/The Cisco Kid - S01E02 - Boomerang.mp4" \
    "https://archive.org/download/CiscoKid_201611/The%20Cisco%20Kid%20-%20Boomerang.mp4" || echo "    ⚠️  Failed to download Cisco Kid S01E02"
else
  echo "    ✅ Cisco Kid S01E02 already exists, skipping download"
fi



echo "�🔧 Setting permissions..."
chmod -R 755 /downloads