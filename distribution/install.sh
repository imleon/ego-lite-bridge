#!/bin/sh
set -eu

BIN="ego-lite-bridge"
MANIFEST_URL="${EGO_LITE_BRIDGE_MANIFEST_URL:-https://raw.githubusercontent.com/imleon/ego-lite-bridge/master/distribution/latest.json}"
INSTALL_DIR="${EGO_LITE_BRIDGE_INSTALL_DIR:-$HOME/.local/bin}"

main() {
    echo ""
    echo "  ego-lite-bridge installer"
    echo ""

    # detect platform
    OS="$(uname -s)"
    case "$OS" in
        Linux)  os="linux" ;;
        Darwin) os="macos" ;;
        *)      err "unsupported OS: $OS" ;;
    esac

    ARCH="$(uname -m)"
    case "$ARCH" in
        x86_64|amd64)   arch="x86_64" ;;
        aarch64|arm64)  arch="aarch64" ;;
        *)              err "unsupported architecture: $ARCH" ;;
    esac

    log "detected ${os}/${arch}"

    # check dependencies
    need curl
    need awk
    if [ "$os" = "linux" ]; then
        need readlink
    fi

    TARGET="${os}-${arch}"
    log "fetching latest release manifest..."
    MANIFEST="$(curl -fsSL --retry 3 --connect-timeout 10 --max-time 20 "$MANIFEST_URL")" \
        || err "can't reach ${MANIFEST_URL}. Please try again later."
    URL="$(printf '%s\n' "$MANIFEST" | awk -v target="\"${TARGET}\"" '
        /^[[:space:]]*"assets"[[:space:]]*:/ { in_assets = 1; next }
        in_assets && /^[[:space:]]*}/ { exit }
        in_assets && index($0, target) {
            sub(/^.*:[[:space:]]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ')"
    SHA256="$(printf '%s\n' "$MANIFEST" | awk -v target="\"${TARGET}\"" '
        /^[[:space:]]*"sha256"[[:space:]]*:/ { in_sha256 = 1; next }
        in_sha256 && /^[[:space:]]*}/ { exit }
        in_sha256 && index($0, target) {
            sub(/^.*:[[:space:]]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ')"
    PRODUCT="$(printf '%s\n' "$MANIFEST" | awk -F '"' '/^[[:space:]]*"product"[[:space:]]*:/ { print $4; exit }')"
    VERSION="$(printf '%s\n' "$MANIFEST" | awk -F '"' '/^[[:space:]]*"version"[[:space:]]*:/ { print $4; exit }')"
    AVAILABLE="$(printf '%s\n' "$MANIFEST" | awk '/^[[:space:]]*"available"[[:space:]]*:/ { gsub(/[ ,]/, "", $2); print $2; exit }')"

    if [ "$PRODUCT" != "ego-lite-bridge" ]; then
        err "release manifest is not for ego-lite-bridge"
    fi
    if [ "$AVAILABLE" != "true" ]; then
        err "ego-lite-bridge release is not available yet"
    fi
    if [ -z "$VERSION" ]; then
        err "release manifest does not include a version"
    fi
    if [ -z "$URL" ]; then
        err "release manifest does not include a binary for ${TARGET}"
    fi
    EXPECTED_URL="https://github.com/imleon/ego-lite-bridge/releases/download/v${VERSION}/ego-lite-bridge-${TARGET}"
    if [ "$URL" != "$EXPECTED_URL" ]; then
        err "release manifest asset URL does not match version ${VERSION} and target ${TARGET}"
    fi
    if [ "${#SHA256}" -ne 64 ]; then
        err "release manifest does not include a valid SHA-256 checksum for ${TARGET}"
    fi
    if ! printf '%s\n' "$SHA256" | awk '/[^0-9A-Fa-f]/ { exit 1 }'; then
        err "release manifest does not include a valid SHA-256 checksum for ${TARGET}"
    fi
    SHA256="$(printf '%s\n' "$SHA256" | awk '{ print tolower($0) }')"

    if command -v sha256sum >/dev/null 2>&1; then
        SHA256_TOOL="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        SHA256_TOOL="shasum"
    elif command -v openssl >/dev/null 2>&1; then
        SHA256_TOOL="openssl"
    else
        err "SHA-256 verification requires sha256sum, shasum, or openssl"
    fi

    log "downloading v${VERSION}..."
    mkdir -p "$INSTALL_DIR"
    SHIM="${INSTALL_DIR}/ego-browser"
    validate_install_paths

    TMP="$(mktemp -d "${INSTALL_DIR}/.${BIN}.XXXXXX")"
    trap 'rm -rf "$TMP"' EXIT
    STAGED_BINARY="${TMP}/${BIN}"

    if ! curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 "$URL" -o "$STAGED_BINARY"; then
        err "download failed from ${URL}"
    fi

    case "$SHA256_TOOL" in
        sha256sum) ACTUAL_SHA256="$(sha256sum < "$STAGED_BINARY" | awk '{ print $1 }')" ;;
        shasum)    ACTUAL_SHA256="$(shasum -a 256 < "$STAGED_BINARY" | awk '{ print $1 }')" ;;
        openssl)   ACTUAL_SHA256="$(openssl dgst -sha256 < "$STAGED_BINARY" | awk '{ print $NF }')" ;;
    esac
    if [ "$ACTUAL_SHA256" != "$SHA256" ]; then
        err "downloaded ego-lite-bridge checksum did not match"
    fi

    # install
    chmod +x "$STAGED_BINARY"
    validate_install_paths
    trap '' PIPE
    CREATED_SHIM=false
    if [ "$os" = "linux" ] && [ ! -L "$SHIM" ]; then
        ln -s "$BIN" "$SHIM"
        CREATED_SHIM=true
    fi
    mv "$STAGED_BINARY" "${INSTALL_DIR}/${BIN}"
    set +e

    if [ "$CREATED_SHIM" = true ]; then
        log "created ego-browser shim at ${SHIM}"
    fi
    log "installed ${BIN} to ${INSTALL_DIR}/${BIN}"

    # check PATH
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            echo ""
            warn "${INSTALL_DIR} is not in your PATH"
            echo "  add it to your shell config:"
            echo ""
            echo "    export PATH=\"${INSTALL_DIR}:\$PATH\""
            echo ""
            ;;
    esac

    # verify
    if command -v "$BIN" >/dev/null 2>&1; then
        echo ""
        log "ready. run '${BIN}' to get started."
    fi

    echo ""
    return 0
}

validate_install_paths() {
    if [ -d "${INSTALL_DIR}/${BIN}" ]; then
        err "installation path is a directory: ${INSTALL_DIR}/${BIN}"
    fi
    if [ "$os" = "linux" ] && { [ -e "$SHIM" ] || [ -L "$SHIM" ]; }; then
        if [ -d "$SHIM" ]; then
            err "shim path is a directory: ${SHIM}"
        fi
        if [ ! -L "$SHIM" ]; then
            err "shim path is not a symlink to ${BIN}: ${SHIM}"
        fi
        SHIM_TARGET="$(readlink "$SHIM"; printf x)"
        if [ "$SHIM_TARGET" != "${BIN}
x" ]; then
            err "shim path is not a symlink to ${BIN}: ${SHIM}"
        fi
    fi
}

log()  { printf '  \033[32m>\033[0m %s\n' "$1"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$1"; }
err()  { printf '  \033[31m✗\033[0m %s\n' "$1" >&2; exit 1; }

need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "requires '$1' — install it first, or download the binary manually from the release page"
    fi
}

main "$@"
