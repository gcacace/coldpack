#!/bin/sh
set -e

REPO="gcacace/coldpack"

detect_platform() {
    os=$(uname -s | tr '[:upper:]' '[:lower:]')
    arch=$(uname -m)

    case "$os" in
        linux)  os="unknown-linux-gnu" ;;
        darwin) os="apple-darwin" ;;
        *)      echo "Error: unsupported OS: $os" >&2; exit 1 ;;
    esac

    case "$arch" in
        x86_64|amd64)   arch="x86_64" ;;
        aarch64|arm64)  arch="aarch64" ;;
        *)              echo "Error: unsupported architecture: $arch" >&2; exit 1 ;;
    esac

    echo "${arch}-${os}"
}

get_latest_tag() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4
    else
        echo "Error: curl or wget is required" >&2
        exit 1
    fi
}

download() {
    url="$1"
    output="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$output" "$url"
    else
        wget -qO "$output" "$url"
    fi
}

install_binary() {
    src="$1"
    if [ -w /usr/local/bin ]; then
        install_dir="/usr/local/bin"
        mv "$src" "$install_dir/coldpack"
        chmod +x "$install_dir/coldpack"
    elif command -v sudo >/dev/null 2>&1; then
        install_dir="/usr/local/bin"
        sudo mv "$src" "$install_dir/coldpack"
        sudo chmod +x "$install_dir/coldpack"
    else
        install_dir="$HOME/.local/bin"
        mkdir -p "$install_dir"
        mv "$src" "$install_dir/coldpack"
        chmod +x "$install_dir/coldpack"
        case ":$PATH:" in
            *":$install_dir:"*) ;;
            *) echo "Warning: $install_dir is not in your PATH. Add it with:" >&2
               echo "  export PATH=\"$install_dir:\$PATH\"" >&2 ;;
        esac
    fi
    echo "$install_dir/coldpack"
}

main() {
    platform=$(detect_platform)
    echo "Detected platform: $platform"

    echo "Fetching latest release..."
    tag=$(get_latest_tag)
    if [ -z "$tag" ]; then
        echo "Error: could not determine latest release" >&2
        exit 1
    fi
    echo "Latest release: $tag"

    archive="coldpack-${tag}-${platform}.tar.gz"
    url="https://github.com/${REPO}/releases/download/${tag}/${archive}"

    tmpdir=$(mktemp -d)
    trap 'rm -rf "$tmpdir"' EXIT

    echo "Downloading $archive..."
    download "$url" "$tmpdir/$archive"

    tar xzf "$tmpdir/$archive" -C "$tmpdir"

    installed_path=$(install_binary "$tmpdir/coldpack")
    echo "Installed coldpack to $installed_path"
    "$installed_path" --version
}

main
