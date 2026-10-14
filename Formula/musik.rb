class Musik < Formula
  desc "Apple Music TUI player"
  homepage "https://github.com/palmachris7/musik"
  # Release tarballs: create a GitHub release (e.g. v0.1.0) and fill the hash:
  #   curl -sL <url> | shasum -a 256
  url "https://github.com/palmachris7/musik/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "66e9bb23443630c5ecfeb9fec91106a20d1d3282515cd20c66ed2bdd10142921"
  license "MIT"
  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "musik", shell_output("#{bin}/musik version")
  end
end
