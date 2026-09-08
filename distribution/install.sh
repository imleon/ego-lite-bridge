#!/bin/sh
set -eu

BIN="ego-lite-bridge"
SKILLS_CLI_VERSION="1.5.24"
MIN_NODE_MAJOR=22
MIN_NODE_MINOR=20
MIN_NODE_VERSION="${MIN_NODE_MAJOR}.${MIN_NODE_MINOR}.0"
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
        need tar
        need node
        need npx
        require_node_version
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
    SKILL_URL="$(printf '%s\n' "$MANIFEST" | awk -F '"' '/^[[:space:]]*"skill_url"[[:space:]]*:/ { print $4; exit }')"
    SKILL_SHA256="$(printf '%s\n' "$MANIFEST" | awk -F '"' '/^[[:space:]]*"skill_sha256"[[:space:]]*:/ { print $4; exit }')"

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
    if [ "$os" = "linux" ]; then
        EXPECTED_SKILL_URL="https://github.com/imleon/ego-lite-bridge/releases/download/v${VERSION}/ego-browser-skill.tgz"
        if [ "$SKILL_URL" != "$EXPECTED_SKILL_URL" ]; then
            err "release manifest skill URL does not match version ${VERSION}"
        fi
        if [ "${#SKILL_SHA256}" -ne 64 ] || ! printf '%s\n' "$SKILL_SHA256" | awk '/[^0-9A-Fa-f]/ { exit 1 }'; then
            err "release manifest does not include a valid skill SHA-256 checksum"
        fi
        SKILL_SHA256="$(printf '%s\n' "$SKILL_SHA256" | awk '{ print tolower($0) }')"
    fi

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
    STAGED_SKILL_ARCHIVE="${TMP}/ego-browser-skill.tgz"
    STAGED_SKILL_DIR="${TMP}/skill"

    if ! curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 "$URL" -o "$STAGED_BINARY"; then
        err "download failed from ${URL}"
    fi

    ACTUAL_SHA256="$(sha256_file "$STAGED_BINARY")"
    if [ "$ACTUAL_SHA256" != "$SHA256" ]; then
        err "downloaded ego-lite-bridge checksum did not match"
    fi

    # prepare and install
    chmod +x "$STAGED_BINARY"
    validate_install_paths
    if [ "$os" = "linux" ]; then
        log "downloading ego-browser skill..."
        if ! curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 "$SKILL_URL" -o "$STAGED_SKILL_ARCHIVE"; then
            err "skill download failed from ${SKILL_URL}"
        fi
        if [ "$(sha256_file "$STAGED_SKILL_ARCHIVE")" != "$SKILL_SHA256" ]; then
            err "downloaded ego-browser skill checksum did not match"
        fi
        validate_skill_archive "$STAGED_SKILL_ARCHIVE"
        mkdir "$STAGED_SKILL_DIR"
        TAR_OPTIONS= tar -xzf "$STAGED_SKILL_ARCHIVE" -C "$STAGED_SKILL_DIR"
        SKILL_SOURCE="${STAGED_SKILL_DIR}/ego-browser"
        if [ ! -f "${SKILL_SOURCE}/SKILL.md" ] || [ ! -f "${SKILL_SOURCE}/references/install.md" ]; then
            err "ego-browser skill archive is missing required files"
        fi
    fi

    trap '' PIPE
    CREATED_SHIM=false
    if [ "$os" = "linux" ] && [ ! -L "$SHIM" ]; then
        ln -s "$BIN" "$SHIM"
        CREATED_SHIM=true
    fi
    if ! mv "$STAGED_BINARY" "${INSTALL_DIR}/${BIN}"; then
        if [ "$CREATED_SHIM" = true ]; then
            rm -f "$SHIM"
        fi
        err "failed to install ${BIN}"
    fi

    if [ "$CREATED_SHIM" = true ]; then
        log "created ego-browser shim at ${SHIM}"
    fi
    log "installed ${BIN} to ${INSTALL_DIR}/${BIN}"

    if [ "$os" = "linux" ]; then
        log "installing ego-browser skill for all agents..."
        if ! npx --yes "skills@${SKILLS_CLI_VERSION}" add "$SKILL_SOURCE" --skill ego-browser --global --agent '*' --yes --copy; then
            err "bridge installed, but ego-browser skill installation failed; partial skill changes were not rolled back"
        fi
        warn "skills CLI may return success despite individual agent installation failures; review its output"
    fi
    set +e

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

sha256_file() {
    case "$SHA256_TOOL" in
        sha256sum) sha256sum < "$1" | awk '{ print $1 }' ;;
        shasum)    shasum -a 256 < "$1" | awk '{ print $1 }' ;;
        openssl)   openssl dgst -sha256 < "$1" | awk '{ print $NF }' ;;
    esac
}

validate_skill_archive() {
    MEMBERS="$(TAR_OPTIONS= tar -tzf "$1")" || err "ego-browser skill archive is invalid"
    [ -n "$MEMBERS" ] || err "ego-browser skill archive is empty"
    if ! printf '%s\n' "$MEMBERS" | awk '
        /^\// { exit 1 }
        { path = $0; sub(/^\.\//, "", path); n = split(path, parts, "/") }
        parts[1] != "ego-browser" { exit 1 }
        { for (i = 1; i <= n; i++) if (parts[i] == "..") exit 1 }
    '; then
        err "ego-browser skill archive has an unsafe path"
    fi
    DETAILS="$(TAR_OPTIONS= tar -tvzf "$1")" || err "ego-browser skill archive is invalid"
    if ! printf '%s\n' "$DETAILS" | awk 'substr($0, 1, 1) !~ /[-d]/ { exit 1 }'; then
        err "ego-browser skill archive contains links or special files"
    fi
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
        SHIM_TARGET="$(readlink -n "$SHIM"; printf x)"
        if [ "$SHIM_TARGET" != "${BIN}x" ]; then
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

require_node_version() {
    NODE_VERSION="$(node --version 2>/dev/null)" || err "requires Node.js ${MIN_NODE_VERSION} or newer"
    NODE_VERSION=${NODE_VERSION#v}
    OLD_IFS=$IFS
    IFS=.
    set -- $NODE_VERSION
    IFS=$OLD_IFS
    if [ "$#" -ne 3 ]; then
        err "requires Node.js ${MIN_NODE_VERSION} or newer"
    fi
    case "$1.$2.$3" in
        *[!0-9.]*|.*|*..*|*.) err "requires Node.js ${MIN_NODE_VERSION} or newer" ;;
    esac
    if [ "$1" -lt "$MIN_NODE_MAJOR" ] || { [ "$1" -eq "$MIN_NODE_MAJOR" ] && [ "$2" -lt "$MIN_NODE_MINOR" ]; }; then
        err "requires Node.js ${MIN_NODE_VERSION} or newer"
    fi
}

main "$@"
