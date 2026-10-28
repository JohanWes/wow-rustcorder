#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Builds the Windows installer inside an MSYS2 UCRT64 shell, from the
# repository root or anywhere else:
#
#   bash windows/build.sh
#
# Needs these MSYS2 packages (prefix mingw-w64-ucrt-x86_64-): gtk4 libadwaita
# gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad rust gcc pkgconf
# meson ninja librsvg, plus curl, tar, make and Inno Setup 6
# (`ISCC.exe` on PATH or in its default folder).
#
# Clapper and a minimal FFmpeg are built once and cached in build-windows/;
# the app folder lands in build-windows/app and the installer beside it,
# with bundle-files.txt listing every shipped file and its size.

set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD
work=$repo/build-windows
prefix=/ucrt64
version=$(sed -n 's/^version = "\(.*\)"/\1/p' native/Cargo.toml | head -1)
mkdir -p "$work"

fetch() { # url sha256 file
    [ -f "$work/$3" ] || curl -fsSL -o "$work/$3" "$1"
    echo "$2  $work/$3" | sha256sum -c --quiet
}

# Same release and options as the Flatpak module, plus the two Windows needs:
# the discoverer feature the Rust bindings link, and exported symbols.
build_clapper() {
    fetch https://github.com/Rafostar/clapper/archive/refs/tags/0.10.0.tar.gz \
        344c0f20e540a63c6fb44cdd5de88c168ed145bb66c1307e79b2b08124780118 clapper-0.10.0.tar.gz
    [ -d "$work/clapper-0.10.0" ] || tar -xzf "$work/clapper-0.10.0.tar.gz" -C "$work"
    [ -f "$prefix/lib/pkgconfig/clapper-gtk-0.0.pc" ] && return
    (
        cd "$work/clapper-0.10.0"
        rm -rf build
        meson setup build --prefix="$prefix" --buildtype=release \
            -Dclapper=enabled -Dclapper-gtk=enabled -Dclapper-app=disabled \
            -Dgst-plugin=enabled -Dintrospection=disabled -Dvapi=disabled -Ddoc=false \
            -Denhancers-loader=disabled -Ddiscoverer=enabled -Dmpris=disabled \
            -Dserver=disabled -Dc_link_args=-Wl,--export-all-symbols
        ninja -C build install
    )
}

# The app only stream-copies (finalize, clip, measure), so one static
# ffmpeg.exe with the few formats it touches is enough.
build_ffmpeg() {
    [ -f "$work/ffmpeg/ffmpeg.exe" ] && return
    fetch https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n8.0.1.tar.gz \
        679aa13a19415d5ddab91e580084e3ab20c963c8240001e5cbb955a97bdd81b1 ffmpeg-n8.0.1.tar.gz
    rm -rf "$work/FFmpeg-n8.0.1"
    tar -xzf "$work/ffmpeg-n8.0.1.tar.gz" -C "$work"
    (
        cd "$work/FFmpeg-n8.0.1"
        ./configure --enable-version3 --enable-static --disable-shared \
            --disable-debug --disable-doc --disable-network --disable-autodetect \
            --disable-everything --disable-ffplay --disable-ffprobe --disable-avdevice \
            --disable-swscale --disable-x86asm --enable-protocol=file,pipe \
            --enable-demuxer=concat,matroska,mov,mpegts --enable-muxer=matroska,mp4 \
            --enable-decoder=aac,h264,hevc --enable-parser=aac,av1,h264,hevc \
            --enable-bsf=aac_adtstoasc,extract_extradata \
            --extra-ldflags=-static
        make -j"$(nproc)"
        mkdir -p "$work/ffmpeg"
        cp ffmpeg.exe COPYING.LGPLv3 "$work/ffmpeg/"
    )
}

build_rust() {
    cargo build --release --manifest-path native/Cargo.toml
    cargo build --release --manifest-path wr-capture/Cargo.toml
}

# GStreamer plugins the player needs: containers, parsers, D3D11 hardware
# video decoders (any GPU that recorded a file can decode it), OpenH264 as
# the software fallback, Media Foundation's AAC decoder, deinterlace (on in
# Clapper by default), scaletempo (audiofx) for playback speed, GL, WASAPI,
# audiomixmatrix to keep stereo on the front speakers of 5.1/7.1 devices.
# gst-libav is left out: MSYS2's FFmpeg drags in ~120 MB of codecs.
plugins=(coreelements playback typefindfunctions isomp4 matroska audioparsers
    videoparsersbad d3d11 openh264 mediafoundation deinterlace audiofx
    audioconvert audioresample volume autodetect wasapi2 audiomixmatrix opengl
    videoconvertscale app pbtypes overlaycomposition clapper)

app=$work/app
copied=()

# Copy a binary and, recursively, every MSYS2 DLL it imports into bin/.
copy_with_deps() { # file destdir
    local name
    name=$(basename "$1")
    [ -f "$2/$name" ] && return
    mkdir -p "$2"
    cp "$1" "$2/"
    copied+=("$1")
    local dll
    for dll in $(objdump -p "$1" | sed -n 's/^\s*DLL Name: //p'); do
        # Mesa's opengl32.dll would shadow the GPU driver's.
        [ "${dll,,}" = opengl32.dll ] && continue
        [ -f "$prefix/bin/$dll" ] && copy_with_deps "$prefix/bin/$dll" "$app/bin"
    done
    return 0
}

bundle() {
    rm -rf "$app"
    copy_with_deps native/target/release/warcraft-rustcorder.exe "$app/bin"
    # Debug symbols are half the binary.
    strip "$app/bin/warcraft-rustcorder.exe"
    copy_with_deps wr-capture/target/release/wr-capture.exe "$app/bin"
    cp "$work/ffmpeg/ffmpeg.exe" "$app/bin/"
    # GLib's D-Bus session bus (single instance) autolaunches through gdbus.
    copy_with_deps "$prefix/bin/gdbus.exe" "$app/bin"

    local plugin
    for plugin in "${plugins[@]}"; do
        copy_with_deps "$prefix/lib/gstreamer-1.0/libgst$plugin.dll" "$app/lib/gstreamer-1.0"
    done
    copy_with_deps "$prefix/libexec/gstreamer-1.0/gst-plugin-scanner.exe" "$app/libexec/gstreamer-1.0"
    for plugin in "$prefix"/lib/clapper-0.0/gst/plugin/importers/*.dll; do
        copy_with_deps "$plugin" "$app/lib/clapper-0.0/gst/plugin/importers"
    done

    # GTK renders SVG icons itself, so librsvg's loader is not needed.
    # The cache lists loaders relative to the install root.
    local loaders=lib/gdk-pixbuf-2.0/2.10.0
    copy_with_deps "$prefix/$loaders/loaders/libpixbufloader-png.dll" "$app/$loaders/loaders"
    gdk-pixbuf-query-loaders "$app/$loaders"/loaders/*.dll |
        sed "s|$(cygpath -m "$app")/||" >"$app/$loaders/loaders.cache"

    mkdir -p "$app/share/glib-2.0/schemas" "$app/share/icons/hicolor/scalable/apps"
    cp "$prefix"/share/glib-2.0/schemas/org.gtk.gtk4.Settings.*.gschema.xml "$app/share/glib-2.0/schemas/"
    glib-compile-schemas "$app/share/glib-2.0/schemas"
    cp -r "$prefix/share/icons/Adwaita" "$app/share/icons/"
    # Without the theme's cursors GTK falls back to the native Windows ones.
    rm -rf "$app/share/icons/Adwaita/cursors"
    cp "$prefix/share/icons/hicolor/index.theme" "$app/share/icons/hicolor/"
    cp data/icons/io.github.JohanWes.WarcraftRustcorder.svg "$app/share/icons/hicolor/scalable/apps/"
    gtk4-update-icon-cache -q -t -f "$app/share/icons/Adwaita" || true

    mkdir -p "$app/share/warcraft-rustcorder"
    cp "$(ls -t native/target/release/build/warcraft-rustcorder-*/out/spells.gresource | head -1)" \
        "$app/share/warcraft-rustcorder/"

    licenses
    check_imports
    (cd "$app" && find . -type f -printf '%s\t%P\n' | sort -t$'\t' -k2) >"$work/bundle-files.txt"
}

# Every DLL a shipped binary imports must ship too or come with Windows.
# Most resolve from bin/; Clapper's importers import the Clapper plugin in
# lib/, which Windows reuses because GStreamer has already loaded it.
check_imports() {
    local -A known=()
    local missing=0 file dll
    while IFS= read -r dll; do known[${dll,,}]=1; done < <(
        find "$app" -type f -iname '*.dll' -printf '%f\n'
        find /c/Windows/System32 -maxdepth 1 -type f -printf '%f\n'
    )
    while IFS= read -r -d '' file; do
        for dll in $(objdump -p "$file" | sed -n 's/^\s*DLL Name: //p'); do
            dll=${dll,,}
            [[ -n ${known[$dll]:-} || $dll == api-ms-win-* || $dll == ext-ms-win-* ]] && continue
            echo "missing: $file imports $dll" >&2
            missing=1
        done
    done < <(find "$app" -type f \( -iname '*.exe' -o -iname '*.dll' \) -print0)
    return "$missing"
}

# License texts for everything shipped: each MSYS2 package that owns a copied
# file, plus the components built here.
licenses() {
    local dir=$app/share/licenses
    mkdir -p "$dir"
    # The repository LICENSE is the legacy app's GPLv2 text; as in the
    # Flatpak, Clapper's GPLv3 text stands in for the app's.
    cp "$work/clapper-0.10.0/COPYING-GPL" "$dir/WarcraftRustcorder-GPL-3.0-or-later"
    mkdir -p "$dir/clapper" "$dir/ffmpeg"
    cp "$work"/clapper-0.10.0/COPYING-* "$dir/clapper/"
    cp "$work/ffmpeg/COPYING.LGPLv3" "$dir/ffmpeg/"
    local packages
    packages=$(printf '%s\n' "${copied[@]}" | grep "^$prefix/" | xargs pacman -Qqo 2>/dev/null | sort -u || true)
    pacman -Q $packages >"$dir/msys2-packages.txt"
    local package
    for package in $packages; do
        local short=${package#mingw-w64-ucrt-x86_64-}
        [ -d "$prefix/share/licenses/$short" ] && cp -r "$prefix/share/licenses/$short" "$dir/"
    done
    return 0
}

installer() {
    local iscc="/c/Program Files (x86)/Inno Setup 6/ISCC.exe"
    [ -x "$iscc" ] || iscc=$(command -v ISCC.exe)
    "$iscc" -Q "-DAppVersion=$version" "-DAppDir=$(cygpath -w "$app")" \
        "-DOutputDir=$(cygpath -w "$work")" "$(cygpath -w windows/installer.iss)"
}

# The update manifest the app reads once it is copied to GitHub Pages.
manifest() {
    local file=WarcraftRustcorder-$version-setup.exe
    local sum size
    sum=$(sha256sum "$work/$file" | cut -d' ' -f1)
    size=$(stat -c%s "$work/$file")
    printf '{"version":"%s","url":"%s","sha256":"%s","size":%s}\n' "$version" \
        "https://github.com/JohanWes/wow-rustcorder/releases/download/v$version/$file" \
        "$sum" "$size" >"$work/latest.json"
}

build_clapper
build_ffmpeg
build_rust
bundle
installer
manifest
du -sh "$app"
ls -la "$work"/*.exe
