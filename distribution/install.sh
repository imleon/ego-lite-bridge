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

    ACTUAL_SHA256="$(sha256_file "$STAGED_BINARY")"
    if [ "$ACTUAL_SHA256" != "$SHA256" ]; then
        err "downloaded ego-lite-bridge checksum did not match"
    fi

    # prepare and install
    chmod +x "$STAGED_BINARY"
    validate_install_paths

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
        # Read the controlling terminal, never the script input (curl | sh).
        if ! skill_interaction_blocked && confirm_skill 2>/dev/null 3<>/dev/tty; then
            install_skill </dev/tty >/dev/tty 2>&1 \
                || skill_err "could not complete the interactive installation"
        else
            warn "ego-browser skill was not installed automatically; see manual installation:"
            echo "  https://github.com/imleon/ego-lite-bridge#optional-agent-skill-installation"
        fi
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

confirm_skill() (
    # Background terminal reads must fail, not suspend the installer. Keep the
    # ignored signal local to this subshell so the skills CLI inherits no change.
    trap '' TTIN
    [ -t 3 ] || return 1
    while :; do
        printf '  Install the ego-browser skill for your agent? (recommended) [Y/n] ' >&3 || return 1
        IFS= read -r ANSWER <&3 || return 1
        case "$ANSWER" in
            ''|[yY]|[yY][eE][sS]) return 0 ;;
            [nN]|[nN][oO]) return 1 ;;
            *) printf '  Please answer yes or no.\n' >&3 || return 1 ;;
        esac
    done
)

skill_err() {
    err "bridge installed; optional ego-browser skill installation incomplete: $1; bridge and partial skill changes were not rolled back"
}

skill_interaction_blocked() {
    # skills@1.5.24 can auto-confirm in agent sessions. Re-audit on upgrades.
    # These are execution signals, not an agent installation directory detector.
    if [ -n "${AI_AGENT:-}${CURSOR_AGENT:-}${GEMINI_CLI:-}${CODEX_SANDBOX:-}${CODEX_CI:-}${CODEX_THREAD_ID:-}${ANTIGRAVITY_AGENT:-}${AUGMENT_AGENT:-}${OPENCODE_CLIENT:-}${CLAUDECODE:-}${CLAUDE_CODE:-}${REPL_ID:-}${COPILOT_MODEL:-}${COPILOT_ALLOW_ALL:-}${COPILOT_GITHUB_TOKEN:-}" ] \
        || [ "${CURSOR_EXTENSION_HOST_ROLE:-}" = "agent-exec" ] || [ -e /opt/.devin ]; then
        warn "agent execution environment detected; run the installer from a normal terminal for interactive skill installation"
        return 0
    fi
    return 1
}

install_skill() {
    for dependency in node npx tar; do
        command -v "$dependency" >/dev/null 2>&1 || skill_err "requires '$dependency' — install it first"
    done
    require_node_version

    SKILL_URL="$(printf '%s\n' "$MANIFEST" | awk -F '"' '/^[[:space:]]*"skill_url"[[:space:]]*:/ { print $4; exit }')" \
        || skill_err "could not read skill URL"
    SKILL_SHA256="$(printf '%s\n' "$MANIFEST" | awk -F '"' '/^[[:space:]]*"skill_sha256"[[:space:]]*:/ { print $4; exit }')" \
        || skill_err "could not read skill checksum"
    EXPECTED_SKILL_URL="https://github.com/imleon/ego-lite-bridge/releases/download/v${VERSION}/ego-browser-skill.tgz"
    if [ "$SKILL_URL" != "$EXPECTED_SKILL_URL" ]; then
        skill_err "release manifest skill URL does not match version ${VERSION}"
    fi
    if [ "${#SKILL_SHA256}" -ne 64 ] || ! printf '%s\n' "$SKILL_SHA256" | awk '/[^0-9A-Fa-f]/ { exit 1 }'; then
        skill_err "release manifest does not include a valid skill SHA-256 checksum"
    fi
    SKILL_SHA256="$(printf '%s\n' "$SKILL_SHA256" | awk '{ print tolower($0) }')" \
        || skill_err "could not normalize skill checksum"

    STAGED_SKILL_ARCHIVE="${TMP}/ego-browser-skill.tgz"
    STAGED_SKILL_DIR="${TMP}/skill"
    log "downloading ego-browser skill..."
    curl -fsSL --connect-timeout 10 --max-time 120 "$SKILL_URL" -o "$STAGED_SKILL_ARCHIVE" \
        || skill_err "skill download failed from ${SKILL_URL}"
    ACTUAL_SKILL_SHA256="$(sha256_file "$STAGED_SKILL_ARCHIVE")" \
        || skill_err "could not checksum ego-browser skill archive"
    [ "$ACTUAL_SKILL_SHA256" = "$SKILL_SHA256" ] \
        || skill_err "downloaded ego-browser skill checksum did not match"
    validate_skill_archive "$STAGED_SKILL_ARCHIVE"
    mkdir "$STAGED_SKILL_DIR" || skill_err "could not create skill staging directory"
    TAR_OPTIONS= tar -xzf "$STAGED_SKILL_ARCHIVE" -C "$STAGED_SKILL_DIR" \
        || skill_err "could not extract ego-browser skill archive"
    SKILL_SOURCE="${STAGED_SKILL_DIR}/ego-browser"
    if [ ! -f "${SKILL_SOURCE}/SKILL.md" ] || [ ! -f "${SKILL_SOURCE}/references/install.md" ]; then
        skill_err "ego-browser skill archive is missing required files"
    fi
    npx --yes "skills@${SKILLS_CLI_VERSION}" add "$SKILL_SOURCE" --skill ego-browser --global --copy \
        || skill_err "skills CLI failed"
    log "skill interactive flow ended; refer to the skills CLI output for the outcome"
}

require_node_version() {
    NODE_VERSION="$(node --version </dev/null 2>/dev/null)" || skill_err "requires Node.js ${MIN_NODE_VERSION} or newer"
    NODE_VERSION=${NODE_VERSION#v}
    OLD_IFS=$IFS
    IFS=.
    set -- $NODE_VERSION
    IFS=$OLD_IFS
    if [ "$#" -ne 3 ]; then
        skill_err "requires Node.js ${MIN_NODE_VERSION} or newer"
    fi
    case "$1.$2.$3" in
        *[!0-9.]*|.*|*..*|*.) skill_err "requires Node.js ${MIN_NODE_VERSION} or newer" ;;
    esac
    if [ "$1" -lt "$MIN_NODE_MAJOR" ] || { [ "$1" -eq "$MIN_NODE_MAJOR" ] && [ "$2" -lt "$MIN_NODE_MINOR" ]; }; then
        skill_err "requires Node.js ${MIN_NODE_VERSION} or newer"
    fi
}

sha256_file() {
    case "$SHA256_TOOL" in
        sha256sum) DIGEST="$(sha256sum < "$1")" || return 1 ;;
        shasum)    DIGEST="$(shasum -a 256 < "$1")" || return 1 ;;
        openssl)   DIGEST="$(openssl dgst -sha256 < "$1")" || return 1 ;;
    esac
    printf '%s\n' "$DIGEST" | awk -v tool="$SHA256_TOOL" '{ print (tool == "openssl" ? $NF : $1) }'
}

validate_skill_archive() {
    MEMBERS="$(TAR_OPTIONS= tar -tzf "$1")" || skill_err "ego-browser skill archive is invalid"
    [ -n "$MEMBERS" ] || skill_err "ego-browser skill archive is empty"
    if ! printf '%s\n' "$MEMBERS" | awk '
        /^\// { exit 1 }
        { path = $0; sub(/^\.\//, "", path); n = split(path, parts, "/") }
        parts[1] != "ego-browser" { exit 1 }
        { for (i = 1; i <= n; i++) if (parts[i] == "..") exit 1 }
    '; then
        skill_err "ego-browser skill archive has an unsafe path"
    fi
    DETAILS="$(TAR_OPTIONS= tar -tvzf "$1")" || skill_err "ego-browser skill archive is invalid"
    if ! printf '%s\n' "$DETAILS" | awk 'substr($0, 1, 1) !~ /[-d]/ { exit 1 }'; then
        skill_err "ego-browser skill archive contains links or special files"
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

main "$@" </dev/null
