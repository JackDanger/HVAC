class Tdorr < Formula
  desc "GPU-accelerated media transcoding that I could figure out"
  homepage "https://github.com/JackDanger/tdorr"
  url "https://github.com/JackDanger/tdorr/archive/v0.1.0.tar.gz"
  sha256 "PLACEHOLDER" # Update with actual sha256 on release
  license "MIT"

  depends_on "rust" => :build
  depends_on "ffmpeg"

  def install
    system "cargo", "install", *std_cargo_args
    etc.install "config.yaml" => "tdorr/config.yaml"
  end

  def caveats
    <<~EOS
      tdorr requires a GPU with hardware HEVC encoding support:
        - macOS: Apple VideoToolbox (built-in on Apple Silicon and recent Intel Macs)
        - Linux: NVIDIA NVENC or Intel VAAPI

      A default config has been installed to:
        #{etc}/tdorr/config.yaml
    EOS
  end

  test do
    assert_match "tdorr", shell_output("#{bin}/tdorr --help")
  end
end
