#!/usr/bin/env bash
# Regenerates the Homebrew formula for a released version.
# Usage: update-tap-formula.sh <version-without-v> <output-path>
# Expects the four unix release archives in the current directory.
set -euo pipefail

VERSION="$1"
OUT="$2"

sha() {
  shasum -a 256 "pdf-minimizer-v${VERSION}-$1.tar.gz" | cut -d' ' -f1
}

SHA_MAC_ARM=$(sha aarch64-apple-darwin)
SHA_MAC_X64=$(sha x86_64-apple-darwin)
SHA_LINUX_ARM=$(sha aarch64-unknown-linux-gnu)
SHA_LINUX_X64=$(sha x86_64-unknown-linux-gnu)

BASE="https://github.com/moujahedkhouja/pdf-minimizer/releases/download/v${VERSION}"

cat > "$OUT" <<EOF
class PdfMinimizer < Formula
  desc "CLI PDF compressor for scanned documents"
  homepage "https://github.com/moujahedkhouja/pdf-minimizer"
  version "${VERSION}"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    if Hardware::CPU.arm?
      url "${BASE}/pdf-minimizer-v${VERSION}-aarch64-apple-darwin.tar.gz"
      sha256 "${SHA_MAC_ARM}"
    else
      url "${BASE}/pdf-minimizer-v${VERSION}-x86_64-apple-darwin.tar.gz"
      sha256 "${SHA_MAC_X64}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "${BASE}/pdf-minimizer-v${VERSION}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_LINUX_ARM}"
    else
      url "${BASE}/pdf-minimizer-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${SHA_LINUX_X64}"
    end
  end

  def install
    bin.install "pdf-minimizer"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/pdf-minimizer --version")
  end
end
EOF
echo "Formula written to $OUT for version ${VERSION}"
