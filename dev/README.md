# Jellyfin Development Environment

A complete Docker Compose setup for testing Jellyswarrm with three preconfigured Jellyfin servers (Movies, TV Shows, Music) and legally downloadable content.

## 🚀 Quick Start

```bash
cd dev
docker-compose up -d
```

What happens:
- Downloads legal sample content automatically
- Starts three Jellyfin servers (movies, tv, music)
- Initializes each server (skips wizard, creates library, ready to browse)

Then access:
- Movies: http://localhost:8096
- TV Shows: http://localhost:8097
- Music: http://localhost:8098

## 👥 Users and libraries

- Each server creates an admin user automatically:
   - Admin: `admin` / `password`
- Libraries are created via API and point to:
   - Movies → `/media/movies`
   - TV Shows → `/media/tv-shows`
   - Music → `/media/music`

Note: Additional non-admin users are not created by default in this setup.


## 📁 Downloaded content

All content is legally downloadable. Current script includes:

- Movies
   - Night of the Living Dead (1968) — Internet Archive (Public Domain)
   - Plan 9 from Outer Space (1959) — Internet Archive (Public Domain)
   - Big Buck Bunny (2008) — Blender Foundation (CC)

- TV Shows
   - The Cisco Kid (1950) — S01E01, S01E02 — Internet Archive (Public Domain)

- Music
   - Kimiko Ishizaka — The Open Goldberg Variations (2012) — OGG — Internet Archive (CC0/PD)
   - Kevin MacLeod — Royalty Free (2017) — MP3 — Internet Archive (CC-BY 3.0; attribution required)
   - Josh Woodward — Breadcrumbs (Instrumental Version) — OGG — Internet Archive Jamendo mirror (CC)

Content is placed under `./data/media/` on the host:

```
data/media/
├── movies/
├── tv-shows/
└── music/
```

## � Permissions and environment

- Containers run with `PUID=1000`, `PGID=1000`, `TZ=UTC` for predictable file ownership and timestamps.
- Media is mounted read-only to Jellyfin servers to avoid accidental writes by the apps.

## 📜 Licenses and attribution

- Public domain items can be used freely.
- CC-BY items (e.g., Kevin MacLeod) require attribution if used or redistributed publicly. Keep attribution in your app/docs if you publish content beyond local testing.

Sources:
- Internet Archive — https://archive.org/
- Blender Foundation — https://www.blender.org/about/projects/