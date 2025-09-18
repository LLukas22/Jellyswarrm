# Jellyfin Development Environment

A complete Docker Compose setup for testing Jellyswarrm with two preconfigured Jellyfin server instances and legally downloadable content.

## 🚀 Quick Start

```bash
cd dev
docker-compose up -d
```

That's it! Docker Compose will:
1. Download legal content automatically
2. Start two preconfigured Jellyfin servers
3. Set up libraries automatically

Then access:
- **Movies Server**: http://localhost:8096 (movies only)
- **TV Shows Server**: http://localhost:8097 (TV series only)

## 👥 Preconfigured Users

Perfect! I've successfully created a development environment with preconfigured users and different passwords for each server:

## ✅ What You Get

**🎬 Movies Server (localhost:8096)** - Dedicated movie library
- **Admin**: `admin` / `password` 
- **User**: `user` / `movies`

**📺 TV Shows Server (localhost:8097)** - Dedicated TV series library  
- **Admin**: `admin` / `password`
- **User**: `user` / `shows`

## 🚀 How to Use

The environment is completely automated:

1. **Start with progress visible**: `docker-compose up` (without -d to see download progress)
2. **Or start in background**: `docker-compose up -d` (then use `docker-compose logs -f content-downloader` to see progress)
3. **Wait for download**: Content downloads automatically (takes a few minutes)
4. **Initialize servers**: `docker-compose --profile init up` (sets up users and libraries)
5. **Access servers**: Both servers start with users already configured
6. **Log in**: Use the credentials above - no setup wizard needed!

The content downloader will download several legally free movies and organize them into appropriate libraries automatically. Both Jellyfin servers are preconfigured to skip the setup wizard and have their libraries ready to go.

## 📁 What's Included

### Content Sources
All content is legally downloadable from:
- **Internet Archive**: Public domain movies and shows
- **Blender Foundation**: Creative Commons licensed films
- **Google Sample Videos**: Test content

### Movies (Public Domain & Creative Commons)
- Night of the Living Dead (1968) - Classic horror, public domain
- Plan 9 from Outer Space (1959) - Sci-fi B-movie, public domain
- The Cabinet of Dr. Caligari (1920) - German expressionist film, public domain
- Big Buck Bunny (2008) - Blender Foundation, CC license
- Sintel (2010) - Blender Foundation, CC license
- Tears of Steel (2012) - Blender Foundation, CC license
- Elephant's Dream (2006) - Blender Foundation, CC license

### TV Shows
- Blender Open Movies - Collection organized as TV series episodes

## 🛠️ Manual Commands

### Start the environment with visible progress
```bash
docker-compose up
```

### Start the environment in background
```bash
docker-compose up -d
```

### Initialize servers (after first startup)
```bash
docker-compose --profile init up
```

### View download progress (if running in background)
```bash
docker-compose logs -f content-downloader
```

### View initialization progress
```bash
docker-compose --profile init logs -f
```

### Stop the environment
```bash
docker-compose down
```

### View logs
```bash
docker-compose logs -f
```

### Restart services
```bash
docker-compose restart
```

### Clean restart (removes volumes)
```bash
docker-compose down -v
docker-compose up -d
```

## 📋 Setup Instructions

1. **Start everything**:
   ```bash
   docker-compose up -d
   ```

2. **Access the servers**:
   - **Movies**: http://localhost:8096 (preconfigured with movie library)
   - **TV Shows**: http://localhost:8097 (preconfigured with TV series library)

3. **Initialize the servers**:
   ```bash
   docker-compose --profile init up
   ```

4. **Both servers are fully configured**:
   - Setup wizard is skipped
   - Libraries are automatically created via API
   - Content is downloaded and ready to browse
   - Users are created automatically

No manual configuration needed!

## 🏗️ Architecture

```
dev/
├── docker-compose.yml          # Main compose file with all services
├── scripts/
│   ├── download-content.sh     # Content download script
│   ├── init-movies-server.sh   # Movies server API initialization
│   └── init-tvshows-server.sh  # TV shows server API initialization
├── data/
│   └── media/                  # Downloaded media files (local folder)
│       ├── movies/
│       ├── tv-shows/
│       └── CONTENT_SUMMARY.txt
└── README.md                   # This file

Docker Volumes:
├── jellyfin-movies-config      # Movie server configuration
├── jellyfin-movies-cache       # Movie server cache
├── jellyfin-tvshows-config     # TV server configuration  
└── jellyfin-tvshows-cache      # TV server cache
```

## 🔧 Configuration

### Servers
- **Movies Server** (port 8096): Preconfigured with movie library pointing to `/media/movies`
- **TV Shows Server** (port 8097): Preconfigured with TV series library pointing to `/media/tv-shows`

### Users
Each server has two preconfigured users:
- **admin/password**: Administrator with full access
- **user/movies** (movies server) or **user/shows** (TV server): Regular user access

### Volumes
- Jellyfin configurations and caches are stored in Docker volumes
- **Media content is stored in `./data/media/`** (local folder on host)
- Content is automatically downloaded on first startup and accessible from host system
- Configuration is done via Jellyfin's REST API (no static config files needed)

### Environment Variables
- `PUID=1000` - User ID for file permissions
- `PGID=1000` - Group ID for file permissions  
- `TZ=UTC` - Timezone

## 🧪 Testing Jellyswarrm

This environment is perfect for testing Jellyswarrm features:

1. **Multiple Server Support**: Two independent Jellyfin instances with different content types
2. **Real Content**: Actual video files with metadata
3. **Specialized Libraries**: One server for movies, one for TV shows
4. **Isolated Environment**: Fully contained in Docker with automatic setup
5. **No Manual Configuration**: Everything is preconfigured and ready to use

## 🐛 Troubleshooting

### Services won't start
```bash
# Check Docker is running
docker info

# Check logs
docker-compose logs
```

### Content download fails
```bash
# Check content downloader logs
docker-compose logs content-downloader

# Retry content download
docker-compose up content-downloader --force-recreate

# Check available space
df -h
```

### Permission issues
```bash
# Check Docker volume permissions
docker-compose exec jellyfin-movies ls -la /config
docker-compose exec jellyfin-tvshows ls -la /config
```

### Port conflicts
If ports 8096 or 8097 are in use, edit `docker-compose.yml`:
```yaml
ports:
  - "8098:8096"  # Change to available port
```

## 📜 Legal Notice

All included content is either:
- **Public Domain**: No copyright restrictions
- **Creative Commons**: Freely redistributable under CC licenses
- **Open Source**: Blender Foundation open movie projects

Sources:
- [Internet Archive](https://archive.org/)
- [Blender Foundation](https://www.blender.org/about/projects/)
- [Google Sample Videos](https://goo.gl/A3JoZX)

## 🤝 Contributing

Feel free to add more legal content sources or improve the setup scripts!