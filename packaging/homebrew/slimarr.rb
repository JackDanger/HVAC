class Slimarr < Formula
  desc "GPU-accelerated media transcoding that I could figure out"
  homepage "https://github.com/JackDanger/slimarr"
  url "https://github.com/JackDanger/slimarr/archive/v0.1.0.tar.gz"
  sha256 "PLACEHOLDER" # Update with actual sha256 on release
  license "MIT"

  depends_on "rust" => :build
  depends_on "ffmpeg"

  def install
    system "cargo", "install", *std_cargo_args
    etc.install "config.yaml" => "slimarr/config.yaml"
  end

  def caveats
    <<~EOS
      slimarr requires a GPU with hardware HEVC encoding support:
        - macOS: Apple VideoToolbox (built-in on Apple Silicon and recent Intel Macs)
        - Linux: NVIDIA NVENC or Intel VAAPI

      A default config has been installed to:
        #{etc}/slimarr/config.yaml
    EOS
  end

  test do
    assert_match "slimarr", shell_output("#{bin}/slimarr --help")
  end
end
