<h1 align="center">Jellyswarrm</h1>

<h3 align="center">Many servers. Single experience.</h3>

<p align="center">
<img alt="Logo Banner" src="./media/banner.svg"/>
<br/>
<br/>
<a href="https://github.com/LLukas22/Jellyswarrm">
<img alt="MIT License" src="https://img.shields.io/github/license/LLukas22/Jellyswarrm.svg"/>
</a>
<a href="https://github.com/LLukas22/Jellyswarrm/releases">
<img alt="Current Release" src="https://img.shields.io/github/release/LLukas22/Jellyswarrm/.svg"/>
</a>
</p>

Jellyswarrm is a powerful proxy service that seamlessly aggregates multiple Jellyfin media servers into a unified interface. Whether you're managing distributed libraries across different locations or simply want to consolidate your media experience, Jellyswarrm makes it effortless to access all your content from a single point.

---


## Local Development
### Getting Started
To get started with development, you'll need to clone the repository along with its submodules. This ensures you have all the necessary components for a complete build:

```bash
git clone --recurse-submodules https://github.com/LLukas22/Jellyswarrm.git
```

If you've already cloned the repository, you can initialize the submodules separately:

```bash
git submodule init
git submodule update
```


<details open>
<summary><strong>Docker</strong></summary>

The quickest way to get Jellyswarrm up and running is with Docker. Simply use the provided [docker-compose](./docker-compose.yml) configuration:

```bash
docker compose up -d
```

This will build and start the application with all necessary dependencies, perfect for both development and production deployments.
</details>



<details>
<summary><strong>Native Build</strong></summary>

For a native development setup, ensure you have both Rust and Node.js installed on your system. 

First, install the UI dependencies. You can use the convenient VS Code task `Install UI Dependencies` from the tasks.json file, or run it manually:

```bash
cd ui
npm install
cd ..
```

Once the dependencies are installed, build the entire project with:

```bash
cargo build --release
```

The build process is streamlined thanks to the included [`build.rs`](./crates/jellyswarrm-proxy/build.rs) script, which automatically compiles the web UI and embeds it into the final binary for a truly self-contained application.
</details>