# Library Merging Feature - Development Setup

## Prerequisites

- Rust toolchain (installed via rustup)
- Node.js (for UI builds)
- Git
- Access to at least one Jellyfin server for testing

---

## 1. Repository Setup

### 1.1 Fork the Repository

```bash
# Via GitHub CLI
gh repo fork LLukas22/Jellyswarrm --clone

# Or manually fork on GitHub, then clone
git clone https://github.com/YOUR_USERNAME/Jellyswarrm.git
cd Jellyswarrm
git submodule init
git submodule update
```

### 1.2 Add Upstream Remote

```bash
git remote add upstream https://github.com/LLukas22/Jellyswarrm.git
git fetch upstream
```

### 1.3 Create Feature Branch

```bash
git checkout -b feature/library-merging
```

---

## 2. Development Instance Setup

### 2.1 Create Separate Data Directory

```bash
# Windows
mkdir dev-instance\data

# Linux/Mac
mkdir -p dev-instance/data
```

### 2.2 Create Development Configuration

Create `dev-instance/data/jellyswarrm.toml`:

```toml
server_id = "dev-jellyswarrm-instance"
public_address = "localhost:3001"
server_name = "Jellyswarrm Dev"
host = "0.0.0.0"
port = 3001
include_server_name_in_media = true
username = "admin"
password = "devpassword"
timeout = 30
ui_route = "ui"
media_streaming_mode = "Redirect"
```

### 2.3 Build and Run Dev Instance

The dev instance uses the `JELLYSWARRM_DATA_DIR` environment variable to specify the data directory.

**Option 1: Use the run-dev.ps1 script (Windows PowerShell)**

```powershell
# Simply run:
.\run-dev.ps1
```

**Option 2: Manual execution**

```powershell
# PowerShell
$env:JELLYSWARRM_DATA_DIR = "F:\code_projects\jellyfin\Jellyswarrm\dev-instance\data"
$env:RUST_LOG = "debug,jellyswarrm_proxy=trace"
cargo run --bin jellyswarrm-proxy
```

```bash
# Bash/Linux
export JELLYSWARRM_DATA_DIR="./dev-instance/data"
export RUST_LOG="debug,jellyswarrm_proxy=trace"
cargo run --bin jellyswarrm-proxy
```

### 2.4 VS Code Launch Configuration

Add to `.vscode/launch.json`:

```json
{
    "version": "0.2.0",
    "configurations": [
        {
            "type": "lldb",
            "request": "launch",
            "name": "Debug Jellyswarrm Dev",
            "cargo": {
                "args": [
                    "build",
                    "--bin=jellyswarrm-proxy",
                    "--package=jellyswarrm-proxy"
                ],
                "filter": {
                    "name": "jellyswarrm-proxy",
                    "kind": "bin"
                }
            },
            "args": ["--data-dir", "dev-instance/data"],
            "cwd": "${workspaceFolder}",
            "env": {
                "RUST_LOG": "debug,jellyswarrm_proxy=trace",
                "JELLYSWARRM_SKIP_UI": "1"
            }
        }
    ]
}
```

---

## 3. Connecting Test Servers

### 3.1 Access Dev Admin UI

Open: `http://localhost:3001/ui`

Login with:
- Username: `admin`
- Password: `devpassword`

### 3.2 Add Your Jellyfin Servers

1. Go to **Servers** section
2. Add your existing servers (same as production):
   - Server A: `https://kaplank.undersphere.se`
   - Server B: `https://jellycopter.undersphere.se`
3. Configure user mappings as needed

---

## 4. Development Workflow

### 4.1 Running Tests

```bash
# Run all tests
cargo test

# Run specific test module
cargo test merged_libraries

# Run with output
cargo test -- --nocapture
```

### 4.2 Hot Reload (Optional)

Install cargo-watch:

```bash
cargo install cargo-watch
```

Run with auto-reload:

```bash
cargo watch -x 'run -- --data-dir dev-instance/data'
```

### 4.3 Code Formatting

```bash
cargo fmt
cargo clippy
```

---

## 5. Production vs Development

| Aspect | Production | Development |
|--------|------------|-------------|
| Port | 3000 | 3001 |
| Data Directory | `data/` | `dev-instance/data/` |
| Log Level | INFO | DEBUG/TRACE |
| Build | Release | Debug |
| Public Address | `jellyswarm.undersphere.se` | `localhost:3001` |

---

## 6. Syncing with Upstream

```bash
# Fetch upstream changes
git fetch upstream

# Rebase your feature branch
git checkout feature/library-merging
git rebase upstream/main

# Push to your fork
git push origin feature/library-merging --force-with-lease
```

---

## 7. Creating the PR

When ready to submit:

```bash
# Ensure all tests pass
cargo test

# Format code
cargo fmt
cargo clippy

# Push feature branch
git push origin feature/library-merging

# Create PR via GitHub CLI
gh pr create --repo LLukas22/Jellyswarrm \
    --title "feat: Add library merging functionality" \
    --body-file docs/features/PR_DESCRIPTION.md
```

---

## 8. Debugging Tips

### 8.1 Enable SQL Query Logging

```bash
export RUST_LOG="debug,sqlx=trace"
```

### 8.2 Inspect Database

```bash
sqlite3 dev-instance/data/jellyswarrm.db
.tables
.schema merged_libraries
SELECT * FROM merged_libraries;
```

### 8.3 Test API Endpoints

```bash
# List merged libraries
curl http://localhost:3001/ui/admin/merged-libraries

# Create merged library
curl -X POST http://localhost:3001/ui/admin/merged-libraries \
    -H "Content-Type: application/json" \
    -d '{"name":"All Movies","collection_type":"movies","dedup_strategy":"provider_ids"}'
```

---

## 9. Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `RUST_LOG` | Log level configuration | `info` |
| `JELLYSWARRM_SKIP_UI` | Skip UI build in cargo | `0` |
| `DATABASE_URL` | SQLite database path | (from config) |

---

## 10. Troubleshooting

### Port Already in Use

```bash
# Find process using port
netstat -ano | findstr :3001  # Windows
lsof -i :3001                  # Linux/Mac

# Kill process
taskkill /PID <pid> /F         # Windows
kill -9 <pid>                  # Linux/Mac
```

### Database Locked

Stop all Jellyswarrm instances before running migrations or direct DB access.

### UI Not Loading

Ensure the UI was built:
```bash
cd ui
npm install
npm run build:production
```

Or skip UI with `JELLYSWARRM_SKIP_UI=1` and access backend directly.
