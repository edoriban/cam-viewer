#!/bin/sh
# cam-viewer installer.
#
# Downloads the latest published release, installs it at ONE canonical path,
# and points the desktop entry at that same path. Copying the binary by hand
# does neither, which is how a stale copy earlier in PATH ends up running
# instead of the version that was just "installed".
#
#   curl -fsSL https://raw.githubusercontent.com/edoriban/cam-viewer/main/install.sh | sh
#
# Installs per-user, so it never needs root.

set -eu

REPO="edoriban/cam-viewer"
BIN_DIR="${HOME}/.local/bin"
BIN="${BIN_DIR}/cam-viewer"
DESKTOP_DIR="${HOME}/.local/share/applications"
DESKTOP="${DESKTOP_DIR}/cam-viewer.desktop"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"

case "$(uname -m)" in
    x86_64|amd64) ;;
    *) die "no published build for $(uname -m); only x86_64 is released" ;;
esac
[ "$(uname -s)" = "Linux" ] || die "this installer is for Linux; on Windows use the .zip asset"

say "Looking up the latest release..."
# Parsed with sed rather than jq, which is not installed everywhere.
TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
      | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      | head -n 1)
[ -n "${TAG}" ] || die "could not determine the latest release tag"

ASSET="cam-viewer-${TAG}-x86_64-linux.tar.gz"
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"

TMP=$(mktemp -d)
# Leaves nothing behind on success, failure, or interrupt.
trap 'rm -rf "${TMP}"' EXIT INT TERM

say "Downloading ${TAG}..."
curl -fsSL "${URL}" -o "${TMP}/${ASSET}" || die "download failed: ${URL}"
tar -xzf "${TMP}/${ASSET}" -C "${TMP}" || die "archive could not be extracted"
[ -f "${TMP}/cam-viewer" ] || die "archive did not contain the expected binary"
chmod +x "${TMP}/cam-viewer"

# Refuse to install something that cannot even report its own version, rather
# than overwriting a working install with a broken download.
"${TMP}/cam-viewer" --version >/dev/null 2>&1 \
    || die "downloaded binary did not run; leaving the existing install alone"

mkdir -p "${BIN_DIR}"
# A running binary cannot be overwritten in place; replacing the path is safe.
mv -f "${TMP}/cam-viewer" "${BIN}"
chmod 755 "${BIN}"
say "Installed: ${BIN}  ($(${BIN} --version))"

mkdir -p "${DESKTOP_DIR}"
cat > "${DESKTOP}" <<DESKTOP_ENTRY
[Desktop Entry]
Type=Application
Name=Cam Viewer
Comment=RTSP camera viewer
Exec=${BIN}
Icon=camera-web
Terminal=false
Categories=Video;Viewer;Utility;
StartupWMClass=cam-viewer
DESKTOP_ENTRY
say "Desktop entry: ${DESKTOP} -> ${BIN}"
command -v update-desktop-database >/dev/null 2>&1 \
    && update-desktop-database "${DESKTOP_DIR}" >/dev/null 2>&1 || true

# The whole point: name every other copy, because one of them may be what the
# shell or the desktop launcher actually runs.
#
# Walks PATH itself rather than using `command -v -a`, which is not a real
# option: `command` rejects it, and the resulting error is easy to swallow,
# leaving the check silently finding nothing.
OTHERS=$(
    IFS=:
    for dir in ${PATH}; do
        [ -n "${dir}" ] || dir="."
        candidate="${dir}/cam-viewer"
        if [ -x "${candidate}" ] && [ "${candidate}" != "${BIN}" ]; then
            printf '%s\n' "${candidate}"
        fi
    done | sort -u
)
if [ -n "${OTHERS}" ]; then
    say ""
    say "WARNING: other cam-viewer copies are on your PATH:"
    printf '%s\n' "${OTHERS}" | while IFS= read -r other; do
        [ -n "${other}" ] || continue
        found=$("${other}" --version 2>/dev/null) || found="older than 0.5.0"
        [ -n "${found}" ] || found="older than 0.5.0"
        say "  ${other}  (${found})"
    done
    say ""
    say "One of those may run instead of the version just installed."
    say "Delete them, then re-run this script."
fi

# Same failure as a stale binary, one layer up: a launcher written by hand
# under another filename keeps pointing at the old path, so the menu icon
# still opens the version that was just replaced.
STALE_ENTRIES=$(
    for dir in "${DESKTOP_DIR}" "${HOME}/.local/share/applications" \
               /usr/share/applications /usr/local/share/applications; do
        [ -d "${dir}" ] || continue
        for entry in "${dir}"/*.desktop; do
            [ -f "${entry}" ] || continue
            [ "${entry}" = "${DESKTOP}" ] && continue
            if grep -q '^Exec=.*cam-viewer' "${entry}" 2>/dev/null; then
                printf '%s\n' "${entry}"
            fi
        done
    done | sort -u
)
if [ -n "${STALE_ENTRIES}" ]; then
    say ""
    say "WARNING: other desktop entries launch cam-viewer:"
    printf '%s\n' "${STALE_ENTRIES}" | while IFS= read -r entry; do
        [ -n "${entry}" ] || continue
        target=$(sed -n 's/^Exec=//p' "${entry}" | head -n 1)
        say "  ${entry}"
        say "      runs: ${target}"
    done
    say ""
    say "Their menu icons still open whatever path they name. Delete them."
fi

case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
        say ""
        say "NOTE: ${BIN_DIR} is not on your PATH."
        say "Add it to your shell profile:  export PATH=\"${BIN_DIR}:\${PATH}\""
        ;;
esac
