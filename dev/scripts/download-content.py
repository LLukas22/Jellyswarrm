import os
import urllib.request
import urllib.error
from pathlib import Path

def download_with_progress(url, filepath):
    """Download a file with progress indication"""
    try:
        print(f"    📥 Downloading to {filepath}...")
        urllib.request.urlretrieve(url, filepath)
        return True
    except urllib.error.URLError as e:
        print(f"    ⚠️  Failed to download: {e}")
        return False

def ensure_directory(path):
    """Create directory if it doesn't exist"""
    Path(path).mkdir(parents=True, exist_ok=True)

def main():
    print("🎬 Starting content download for Jellyfin development servers...")
    
    # Create directory structure
    print("📁 Creating directory structure...")
    downloads_base = Path("/downloads")
    movies_dir = downloads_base / "movies"
    tv_shows_dir = downloads_base / "tv-shows"
    music_dir = downloads_base / "music"
    
    ensure_directory(movies_dir)
    ensure_directory(tv_shows_dir)
    ensure_directory(music_dir)
    
    print("🎭 Downloading public domain movies from Internet Archive...")
    
    # Night of the Living Dead (1968) - Public Domain
    print("  📥 Night of the Living Dead (1968)...")
    night_dir = movies_dir / "Night of the Living Dead (1968)"
    night_file = night_dir / "Night of the Living Dead (1968).mp4"
    ensure_directory(night_dir)
    
    if not night_file.exists():
        download_with_progress(
            "https://archive.org/download/night_of_the_living_dead_dvd/Night.mp4",
            night_file
        )
    else:
        print("    ✅ Night of the Living Dead already exists, skipping download")
    
    # Plan 9 from Outer Space (1959) - Public Domain
    print("  📥 Plan 9 from Outer Space (1959)...")
    plan9_dir = movies_dir / "Plan 9 from Outer Space (1959)"
    plan9_file = plan9_dir / "Plan 9 from Outer Space (1959).mp4"
    ensure_directory(plan9_dir)
    
    if not plan9_file.exists():
        download_with_progress(
            "https://archive.org/download/plan-9-from-outer-space-1959_ed-wood/PLAN%209%20FROM%20OUTER%20SPACE%201959.ia.mp4",
            plan9_file
        )
    else:
        print("    ✅ Plan 9 from Outer Space already exists, skipping download")
    
    print("🎨 Downloading Creative Commons content...")
    
    # Big Buck Bunny
    print("  📥 Big Buck Bunny...")
    bunny_dir = movies_dir / "Big Buck Bunny (2008)"
    bunny_file = bunny_dir / "Big Buck Bunny (2008).mp4"
    ensure_directory(bunny_dir)
    
    if not bunny_file.exists():
        download_with_progress(
            "https://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
            bunny_file
        )
    else:
        print("    ✅ Big Buck Bunny already exists, skipping download")
    
    print("📺 Downloading public domain TV series...")
    
    # The Cisco Kid - Public Domain Western Series
    print("  📥 The Cisco Kid (1950-1956)...")
    cisco_dir = tv_shows_dir / "The Cisco Kid (1950)" / "Season 01"
    ensure_directory(cisco_dir)
    
    # Episode 1
    ep1_file = cisco_dir / "The Cisco Kid - S01E01 - The Gay Caballero.mp4"
    if not ep1_file.exists():
        download_with_progress(
            "https://archive.org/download/TheCiscoKidpublicdomain/The_Cisco_Kid_s01e01.mp4",
            ep1_file
        )
    else:
        print("    ✅ Cisco Kid S01E01 already exists, skipping download")
    
    # Episode 2
    ep2_file = cisco_dir / "The Cisco Kid - S01E02 - Boomerang.mp4"
    if not ep2_file.exists():
        download_with_progress(
            "https://archive.org/download/TheCiscoKidpublicdomain/The_Cisco_Kid_s01e02.mp4",
            ep2_file
        )
    else:
        print("    ✅ Cisco Kid S01E02 already exists, skipping download")
    
    print("🎵 Downloading royalty-free and freely-copiable music albums...")

    # Album 1: The Open Goldberg Variations (2012) — Kimiko Ishizaka (CC0/Public Domain)
    # Source: https://archive.org/details/The_Open_Goldberg_Variations-11823
    print("  🎹 Downloading 'The Open Goldberg Variations' (Kimiko Ishizaka)...")
    ogv_dir = music_dir / "Kimiko Ishizaka" / "The Open Goldberg Variations (2012)"
    ensure_directory(ogv_dir)

    ogv_tracks = [
        ("01 - Aria.ogg", "Kimiko_Ishizaka_-_01_-_Aria.ogg"),
        ("02 - Variatio 1 a 1 Clav.ogg", "Kimiko_Ishizaka_-_02_-_Variatio_1_a_1_Clav.ogg"),
        ("03 - Variatio 2 a 1 Clav.ogg", "Kimiko_Ishizaka_-_03_-_Variatio_2_a_1_Clav.ogg"),
        ("04 - Variatio 3 a 1 Clav. Canone all'Unisuono.ogg", "Kimiko_Ishizaka_-_04_-_Variatio_3_a_1_Clav_Canone_allUnisuono.ogg"),
    ]
    for display_name, src_name in ogv_tracks:
        target = ogv_dir / display_name
        if not target.exists():
            download_with_progress(
                f"https://archive.org/download/The_Open_Goldberg_Variations-11823/{src_name}",
                target
            )
        else:
            print(f"    ✅ {display_name} already exists, skipping")

    # Album 2: Kevin MacLeod — Royalty Free (2017) (CC-BY 3.0 — attribution required)
    # Source: https://archive.org/details/Kevin-MacLeod_Royalty-Free_2017_FullAlbum
    print("  🎼 Downloading 'Kevin MacLeod: Royalty Free (2017)'...")
    kml_dir = music_dir / "Kevin MacLeod" / "Royalty Free (2017)"
    ensure_directory(kml_dir)

    # Filenames on IA are simple track names under VBR MP3; no "Kevin MacLeod - 00 -" prefix
    kml_tracks = [
        ("01 - Achaidh Cheide.mp3", "Achaidh%20Cheide.mp3"),
        ("02 - Achilles.mp3", "Achilles.mp3"),
    ]
    for display_name, src_name in kml_tracks:
        target = kml_dir / display_name
        if not target.exists():
            download_with_progress(
                f"https://archive.org/download/Kevin-MacLeod_Royalty-Free_2017_FullAlbum/{src_name}",
                target
            )
        else:
            print(f"    ✅ {display_name} already exists, skipping")

    # Album 3: Josh Woodward — Breadcrumbs (Instrumental Version) (CC — Jamendo archive)
    # Source: https://archive.org/details/jamendo-089689
    print("  🎵 Downloading 'Josh Woodward: Breadcrumbs (Instrumental Version)'...")
    jw_dir = music_dir / "Josh Woodward" / "Breadcrumbs (Instrumental Version)"
    ensure_directory(jw_dir)

    # We'll fetch first three tracks to keep it small
    for idx in [1, 2, 3]:
        src = f"https://archive.org/download/jamendo-089689/{idx:02}.ogg"
        target = jw_dir / f"{idx:02}.ogg"
        if not target.exists():
            download_with_progress(src, target)
        else:
            print(f"    ✅ Track {idx:02}.ogg already exists, skipping")

    print("🎉 Content download completed!")

if __name__ == "__main__":
    main()