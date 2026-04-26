#!/bin/bash
# Publishes the PKGBUILD to the AUR.
# Usage: ./aur-publish.sh [commit message]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AUR_PKGNAME="ioexplorer-git"
AUR_REPO="ssh://aur@aur.archlinux.org/${AUR_PKGNAME}.git"
AUR_DIR="${SCRIPT_DIR}/.aur-repo"

# Default commit message
COMMIT_MSG="${1:-Update PKGBUILD}"

# Ensure PKGBUILD exists
if [ ! -f "${SCRIPT_DIR}/PKGBUILD" ]; then
    echo "Error: PKGBUILD not found in ${SCRIPT_DIR}"
    exit 1
fi

# Ensure makepkg is available (for generating .SRCINFO)
if ! command -v makepkg &>/dev/null; then
    echo "Error: makepkg is required but not found."
    exit 1
fi

# Clone or update the AUR repo
if [ -d "${AUR_DIR}/.git" ]; then
    echo "Pulling latest from AUR..."
    git -C "${AUR_DIR}" pull --rebase || true
else
    echo "Cloning AUR repo..."
    git clone "${AUR_REPO}" "${AUR_DIR}" 2>/dev/null || {
        # If the repo doesn't exist yet on AUR, initialize it
        echo "AUR repo not found remotely. Initializing new repo..."
        mkdir -p "${AUR_DIR}"
        git -C "${AUR_DIR}" init
        git -C "${AUR_DIR}" remote add origin "${AUR_REPO}"
    }
fi

# Copy PKGBUILD into the AUR repo
cp "${SCRIPT_DIR}/PKGBUILD" "${AUR_DIR}/PKGBUILD"

# Resolve the real version from the project git history and patch the AUR copy.
# The static pkgver=r0.000000 in PKGBUILD is intentional for the source repo;
# makepkg --printsrcinfo can't run pkgver() here because $srcdir doesn't exist.
GENERATED_VERSION=$(printf "r%s.%s" \
    "$(git -C "${SCRIPT_DIR}" rev-list --count HEAD)" \
    "$(git -C "${SCRIPT_DIR}" rev-parse --short HEAD)")
echo "Package version: ${GENERATED_VERSION}"
sed -i "s/^pkgver=.*/pkgver=${GENERATED_VERSION}/" "${AUR_DIR}/PKGBUILD"

# Generate .SRCINFO
echo "Generating .SRCINFO..."
(cd "${AUR_DIR}" && makepkg --printsrcinfo > .SRCINFO)

# Stage, commit, push
cd "${AUR_DIR}"
git add PKGBUILD .SRCINFO
if git diff --cached --quiet; then
    echo "No changes to publish."
    exit 0
fi
git commit -m "${COMMIT_MSG}"
git push origin master

echo "Published ${AUR_PKGNAME} to AUR."

# Cleanup
rm -rf "${AUR_DIR}"
echo "Cleaned up temporary AUR repo."
