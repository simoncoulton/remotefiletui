#!/usr/bin/env bash
# Usage: gen-formula.sh <version> <sha-macos-arm> <sha-macos-intel> <sha-linux-arm> <sha-linux-intel>
# Writes Formula/remotefiletui.rb pointing at the binaries attached to the GitHub release.
set -euo pipefail
version=$1
base="https://github.com/simoncoulton/remotefiletui/releases/download/v${version}/remotefiletui-v${version}"

cat > "$(dirname "$0")/../Formula/remotefiletui.rb" <<RUBY
class Remotefiletui < Formula
  desc "Terminal UI for browsing and transferring files over SFTP, FTP(S) and S3"
  homepage "https://github.com/simoncoulton/remotefiletui"
  version "${version}"
  license "MIT"

  on_macos do
    on_arm do
      url "${base}-aarch64-apple-darwin.tar.gz"
      sha256 "$2"
    end
    on_intel do
      url "${base}-x86_64-apple-darwin.tar.gz"
      sha256 "$3"
    end
  end

  on_linux do
    on_arm do
      url "${base}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$4"
    end
    on_intel do
      url "${base}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$5"
    end
  end

  def install
    bin.install "remotefiletui"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/remotefiletui --version")
  end
end
RUBY
