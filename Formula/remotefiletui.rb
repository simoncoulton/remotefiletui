class Remotefiletui < Formula
  desc "Terminal UI for browsing and transferring files over SFTP, FTP(S) and S3"
  homepage "https://github.com/simoncoulton/remotefiletui"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/simoncoulton/remotefiletui/releases/download/v0.1.0/remotefiletui-v0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "7ce1c0824a79338c1d7ca51fcfaa86205b1f3a6ce9154cf95f092cd02ff0487b"
    end
    on_intel do
      url "https://github.com/simoncoulton/remotefiletui/releases/download/v0.1.0/remotefiletui-v0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "09fa659bd280e00e6debec5637380bc888fddc106fb41386c1b44ad0e40b4a71"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/simoncoulton/remotefiletui/releases/download/v0.1.0/remotefiletui-v0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "39785e71f77b1b462a1a2c1f345a5445a0103f5905f7fd7c0cbfc69d7b86e218"
    end
    on_intel do
      url "https://github.com/simoncoulton/remotefiletui/releases/download/v0.1.0/remotefiletui-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "9ea64562410e001ef9b3aeb202df3ebfeefa1c249e75a41eda19bd154de8f2b8"
    end
  end

  def install
    bin.install "remotefiletui"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/remotefiletui --version")
  end
end
