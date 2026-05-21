VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
APPDIR  := target/th420-display.AppDir
APPOUT  := th420-display-$(VERSION)-x86_64.AppImage

# appimagetool: use system binary or download to target/
APPIMAGETOOL := $(shell command -v appimagetool 2>/dev/null)
ifeq ($(APPIMAGETOOL),)
APPIMAGETOOL := target/appimagetool
endif

.PHONY: all release appimage clean

all: release

release:
	cargo build --release
	cargo build --release --features gui

# ── AppImage ──────────────────────────────────────────────────────────────────

target/appimagetool:
	@echo "Downloading appimagetool..."
	@mkdir -p target
	curl -fsSL -o $@ \
	  https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
	chmod +x $@

appimage: release $(APPIMAGETOOL)
	@rm -rf $(APPDIR)
	@mkdir -p $(APPDIR)/usr/bin

	cp target/release/th420-config   $(APPDIR)/usr/bin/
	cp target/release/th420-display  $(APPDIR)/usr/bin/
	cp assets/th420-config.png       $(APPDIR)/th420-config.png
	cp assets/th420-config.png       $(APPDIR)/.DirIcon

	@{ echo '[Desktop Entry]'; \
	   echo 'Name=TH420 Display Config'; \
	   echo 'Comment=CPU/GPU monitor configurator for Thermaltake TH420 V2 LCD'; \
	   echo 'Exec=th420-config'; \
	   echo 'Icon=th420-config'; \
	   echo 'Type=Application'; \
	   echo 'Categories=System;Monitor;'; \
	   echo 'Terminal=false'; \
	   echo 'StartupNotify=true'; \
	   echo 'StartupWMClass=th420-config'; \
	} > $(APPDIR)/th420-config.desktop

	@{ echo '#!/bin/bash'; \
	   echo 'exec "$$APPDIR/usr/bin/th420-config" "$$@"'; \
	} > $(APPDIR)/AppRun
	@chmod +x $(APPDIR)/AppRun

	ARCH=x86_64 $(APPIMAGETOOL) $(APPDIR) $(APPOUT)

	@echo ""
	@echo "  Built: $(APPOUT)"
	@echo ""
	@echo "  First-time udev setup (once, needs sudo):"
	@echo "    sudo ./install-udev.sh"
	@echo ""
	@echo "  Run:"
	@echo "    chmod +x $(APPOUT) && ./$(APPOUT)"

clean:
	cargo clean
	rm -rf $(APPDIR) th420-display-*.AppImage
