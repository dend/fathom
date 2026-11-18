<div align="center">
	<br>
	<br>
	<img src="assets/icons/icon.png" alt="Fathom" height="200">
	<br>
	<br>
	<h1>Fathom</h1>
	<b>High performance Cloudflare R2 storage manager</b>
	<br>
	<br>
	<p>
		<a href="https://github.com/dend/fathom/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/dend/fathom/ci.yml?branch=main&label=CI"></a>
		<a href="https://github.com/dend/fathom/actions/workflows/release.yml"><img alt="Release build" src="https://img.shields.io/github/actions/workflow/status/dend/fathom/release.yml?label=release%20build"></a>
		<a href="https://github.com/dend/fathom/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/dend/fathom?sort=semver"></a>
		<img alt="Platforms" src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue">
		<a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/dend/fathom"></a>
	</p>
	<br>
</div>

<img src="assets/screenshots/fathom.png" alt="Fathom showing a folder of files with the file menu open">

- Browse buckets and folders, even ones with tens of thousands of files.
- Upload and download in bulk, or drag files onto the window.
- Rename, delete, copy and move files across folders and buckets.
- Preview images and text, and edit text files in place.
- Copy a file's public URL on your custom domain, or a presigned URL.

## Install

1. Download the installer for your platform from the [latest release](https://github.com/dend/fathom/releases/latest).
2. Run it. On macOS, then run `xattr -dr com.apple.quarantine /Applications/Fathom.app`.
3. To update later, run the newer installer.

## Connect to R2

1. Create an API token in the [Cloudflare dashboard](https://dash.cloudflare.com/?to=/:account/r2/api-tokens), under **R2 > Manage API tokens**.
2. In Fathom, choose **Add connection...** and paste your **Account ID** and the token's **Token value**.

## Build

```sh
cargo run
```

On Linux, install `libdbus-1-dev` and `pkg-config` first.

## License

[MIT](LICENSE)
