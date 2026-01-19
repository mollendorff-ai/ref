# Möllendorff Ref - Reference verification toolkit
# Build targets for optimized static binaries

.PHONY: help build install install-release uninstall lint format test clean pre-commit
.PHONY: build-linux build-linux-arm64 build-windows build-all
.PHONY: release-linux release-linux-arm64 release-windows release-all
.PHONY: deploy-kveldulf

# ═══════════════════════════════════════════════════════════════════════════════
# CROSS-COMPILATION TARGETS
# ═══════════════════════════════════════════════════════════════════════════════

TARGET_LINUX       := x86_64-unknown-linux-musl
TARGET_LINUX_ARM64 := aarch64-unknown-linux-gnu
TARGET_WINDOWS     := x86_64-pc-windows-gnu

BIN_LINUX       := target/$(TARGET_LINUX)/release/ref
BIN_LINUX_ARM64 := target/$(TARGET_LINUX_ARM64)/release/ref
BIN_WINDOWS     := target/$(TARGET_WINDOWS)/release/ref.exe

# Output directory for release binaries
DIST_DIR := dist

# Install directory (~/bin on all machines)
INSTALL_DIR := $(HOME)/bin

# Tool detection
HAS_UPX := $(shell command -v upx 2> /dev/null)

# Remote deploy target
KVELDULF := kveldulf

# ═══════════════════════════════════════════════════════════════════════════════
# HELP
# ═══════════════════════════════════════════════════════════════════════════════

help:
	@echo "Möllendorff Ref - Build Commands"
	@echo ""
	@echo "Build & Install:"
	@echo "  make build              - Build for current platform"
	@echo "  make install            - Build and install to $(INSTALL_DIR)"
	@echo "  make install-release    - Install release binary (from dist/)"
	@echo "  make uninstall          - Remove from $(INSTALL_DIR)"
	@echo ""
	@echo "Cross-Compilation:"
	@echo "  make build-linux        - Linux x86_64 (musl static)"
	@echo "  make build-linux-arm64  - Linux ARM64"
	@echo "  make build-windows      - Windows x86_64"
	@echo "  make build-all          - All targets"
	@echo ""
	@echo "Release (build + UPX → dist/):"
	@echo "  make release-linux      - Linux x86_64 + UPX"
	@echo "  make release-linux-arm64- Linux ARM64"
	@echo "  make release-windows    - Windows x86_64"
	@echo "  make release-all        - All release binaries"
	@echo ""
	@echo "Deploy:"
	@echo "  make deploy-kveldulf    - Build and deploy to kveldulf"
	@echo ""
	@echo "Code Quality:"
	@echo "  make lint / format / test / pre-commit"
	@echo ""
	@echo "  make clean              - Remove build artifacts"

# ═══════════════════════════════════════════════════════════════════════════════
# NATIVE BUILD
# ═══════════════════════════════════════════════════════════════════════════════

build:
	@echo "Building release binary..."
	@cargo build --release
	@echo "Binary: target/release/ref"
	@ls -lh target/release/ref

install: build
	@mkdir -p $(INSTALL_DIR)
	@install -m 755 target/release/ref $(INSTALL_DIR)/ref
	@echo "Installed to $(INSTALL_DIR)/ref"

install-release: $(DIST_DIR)/ref-linux-x86_64
	@mkdir -p $(INSTALL_DIR)
	@install -m 755 $(DIST_DIR)/ref-linux-x86_64 $(INSTALL_DIR)/ref
	@echo "Installed to $(INSTALL_DIR)/ref"

uninstall:
	@rm -f $(INSTALL_DIR)/ref

# ═══════════════════════════════════════════════════════════════════════════════
# CROSS-COMPILATION BUILDS
# ═══════════════════════════════════════════════════════════════════════════════

build-linux:
	@echo "Building for Linux x86_64 (musl static)..."
	@cargo build --release --target $(TARGET_LINUX)
	@ls -lh $(BIN_LINUX)

build-linux-arm64:
	@echo "Building for Linux ARM64..."
	@cargo build --release --target $(TARGET_LINUX_ARM64)
	@ls -lh $(BIN_LINUX_ARM64)

build-windows:
	@echo "Building for Windows x86_64..."
	@cargo build --release --target $(TARGET_WINDOWS)
	@ls -lh $(BIN_WINDOWS)

build-all: build-linux build-linux-arm64 build-windows
	@echo "All cross-compile targets built."

# ═══════════════════════════════════════════════════════════════════════════════
# RELEASE BUILDS (with UPX compression where supported)
# ═══════════════════════════════════════════════════════════════════════════════

$(DIST_DIR):
	@mkdir -p $(DIST_DIR)

release-linux: build-linux $(DIST_DIR)
	@cp $(BIN_LINUX) $(DIST_DIR)/ref-linux-x86_64
ifdef HAS_UPX
	@echo "Compressing with UPX..."
	@upx --best --lzma $(DIST_DIR)/ref-linux-x86_64
endif
	@ls -lh $(DIST_DIR)/ref-linux-x86_64

release-linux-arm64: build-linux-arm64 $(DIST_DIR)
	@cp $(BIN_LINUX_ARM64) $(DIST_DIR)/ref-linux-arm64
	@echo "Note: UPX not supported for ARM64"
	@ls -lh $(DIST_DIR)/ref-linux-arm64

release-windows: build-windows $(DIST_DIR)
	@cp $(BIN_WINDOWS) $(DIST_DIR)/ref-windows-x86_64.exe
	@echo "Note: UPX skipped for Windows (antivirus false positives)"
	@ls -lh $(DIST_DIR)/ref-windows-x86_64.exe

release-all: release-linux release-linux-arm64 release-windows
	@echo ""
	@echo "Release binaries in $(DIST_DIR)/:"
	@ls -lh $(DIST_DIR)/

# ═══════════════════════════════════════════════════════════════════════════════
# REMOTE DEPLOY
# ═══════════════════════════════════════════════════════════════════════════════

deploy-kveldulf:
	@echo "Deploying to $(KVELDULF)..."
	@echo "  1. Syncing source..."
	@rsync -az --delete --exclude='target/' --exclude='.git/' . $(KVELDULF):~/src/ref/
	@echo "  2. Building on $(KVELDULF)..."
	@ssh $(KVELDULF) "cd ~/src/ref && cargo build --release"
	@echo "  3. Installing to ~/bin..."
	@ssh $(KVELDULF) "mkdir -p ~/bin && install -m 755 ~/src/ref/target/release/ref ~/bin/ref"
	@echo "  4. Verifying..."
	@ssh $(KVELDULF) "~/bin/ref --version"
	@echo "Deployed successfully!"

# ═══════════════════════════════════════════════════════════════════════════════
# CODE QUALITY
# ═══════════════════════════════════════════════════════════════════════════════

lint:
	@cargo clippy --all-targets -- -D warnings

format:
	@cargo fmt

test:
	@cargo test

pre-commit: format lint test

# ═══════════════════════════════════════════════════════════════════════════════
# HOUSEKEEPING
# ═══════════════════════════════════════════════════════════════════════════════

clean:
	@cargo clean
	@rm -rf $(DIST_DIR)
