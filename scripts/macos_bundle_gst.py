#!/usr/bin/env python3
"""Copia GStreamer (solo los plugins que usa TransmitIA) dentro de TransmitIA.app
y reescribe las rutas de las librerías para que la app no dependa de Homebrew.

Uso: python3 scripts/macos_bundle_gst.py ruta/a/TransmitIA.app

Estructura resultante (codesign rechaza subcarpetas en Frameworks y MacOS; los
plugins van en Resources ya firmados uno por uno, como los módulos de Electron):
  Contents/MacOS/transmitia                       (rpath @executable_path/../Frameworks)
  Contents/MacOS/gst-plugin-scanner
  Contents/Resources/gstreamer-1.0/*.dylib        (plugins)
  Contents/Resources/gio/modules/libgiognutls.so  (TLS para RTMPS)
  Contents/Frameworks/*.dylib                  (dependencias, id @rpath/<nombre>)
  Contents/Resources/etc/fonts/fonts.conf      (si se empaqueta fontconfig)
"""

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# Plugins que usa la app en macOS (ver media.rs/engine.rs). Mantener al día.
PLUGINS = [
    "app", "applemedia", "audioconvert", "audiomixer", "audioparsers", "audioresample",
    "audiotestsrc", "closedcaption", "compositor", "coreelements", "debug", "faac", "flv",
    "imagefreeze", "jpeg", "libav", "matroska", "mpegtsmux", "osxaudio", "pango", "png",
    "rsclosedcaption", "rtmp2", "srt", "typefindfunctions", "videobox", "videoconvertscale",
    "videoparsersbad", "videorate", "videotestsrc", "volume", "x264",
]

EXTERNAL_PREFIXES = ("/opt/homebrew/", "/usr/local/")


def sh(*args: str) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def brew_prefix(formula: str) -> Path:
    return Path(sh("brew", "--prefix", formula).strip())


def deps(binary: Path) -> list[str]:
    out = sh("otool", "-L", str(binary)).splitlines()[1:]
    return [line.strip().split(" (")[0] for line in out if line.strip()]


def rpaths(binary: Path) -> list[str]:
    out = sh("otool", "-l", str(binary))
    return re.findall(r"cmd LC_RPATH\n\s+cmdsize \d+\n\s+path (.+?) \(offset", out)


def resolve(dep: str, owner_src: Path) -> Path | None:
    """Ruta real de una dependencia externa (o None si es del sistema)."""
    if dep.startswith(EXTERNAL_PREFIXES):
        return Path(dep).resolve()
    name = dep.split("/", 1)[1] if "/" in dep else dep
    if dep.startswith("@rpath/"):
        for rp in rpaths(owner_src):
            base = rp.replace("@loader_path", str(owner_src.parent))
            cand = Path(base) / name
            if cand.exists():
                return cand.resolve()
        cand = Path("/opt/homebrew/lib") / name
        return cand.resolve() if cand.exists() else None
    if dep.startswith("@loader_path/"):
        cand = owner_src.parent / dep[len("@loader_path/"):]
        return cand.resolve() if cand.exists() else None
    return None


def main() -> None:
    app = Path(sys.argv[1]).resolve()
    macos = app / "Contents/MacOS"
    fw = app / "Contents/Frameworks"
    res = app / "Contents/Resources"
    plugdir = res / "gstreamer-1.0"
    giodir = res / "gio/modules"
    for d in (fw, plugdir, giodir):
        d.mkdir(parents=True, exist_ok=True)

    gst = brew_prefix("gstreamer")
    plugin_src = gst / "lib/gstreamer-1.0"
    scanner_src = gst / "libexec/gstreamer-1.0/gst-plugin-scanner"
    gio_src = brew_prefix("glib-networking") / "lib/gio/modules/libgiognutls.so"

    exe = macos / "transmitia"

    # (archivo dentro del bundle, archivo original de donde vino, rpath a agregar)
    targets: list[tuple[Path, Path, str]] = [(exe, exe, "@executable_path/../Frameworks")]
    for name in PLUGINS:
        src = plugin_src / f"libgst{name}.dylib"
        if not src.exists():
            sys.exit(f"falta el plugin {src}")
        dst = plugdir / src.name
        shutil.copy2(src, dst)
        targets.append((dst, src, "@loader_path/../../Frameworks"))
    scanner_dst = macos / "gst-plugin-scanner"
    shutil.copy2(scanner_src, scanner_dst)
    targets.append((scanner_dst, scanner_src, "@executable_path/../Frameworks"))
    gio_dst = giodir / gio_src.name
    shutil.copy2(gio_src, gio_dst)
    targets.append((gio_dst, gio_src, "@loader_path/../../../Frameworks"))

    copied: dict[str, Path] = {}  # nombre → original
    queue = list(targets)
    changes: dict[Path, list[tuple[str, str]]] = {}
    while queue:
        dst, src, _ = queue.pop()
        for dep in deps(dst):
            real = resolve(dep, src)
            if real is None:
                continue
            name = Path(dep).name
            changes.setdefault(dst, []).append((dep, f"@rpath/{name}"))
            if name not in copied:
                copied[name] = real
                lib_dst = fw / name
                shutil.copy2(real, lib_dst)
                os.chmod(lib_dst, 0o755)
                queue.append((lib_dst, real, "@loader_path"))
                targets.append((lib_dst, real, "@loader_path"))

    for dst, _src, rpath in targets:
        os.chmod(dst, 0o755)
        args = ["install_name_tool"]
        if dst.suffix in (".dylib", ".so"):
            args += ["-id", f"@rpath/{dst.name}"]
        for old, new in changes.get(dst, []):
            args += ["-change", old, new]
        for rp in rpaths(dst):
            if rp.startswith(EXTERNAL_PREFIXES) or "homebrew" in rp:
                args += ["-delete_rpath", rp]
        if rpath not in rpaths(dst):
            args += ["-add_rpath", rpath]
        args.append(str(dst))
        subprocess.run(args, check=True, capture_output=True)

    # fontconfig (si quedó empaquetado) necesita un fonts.conf propio.
    if any(n.startswith("libfontconfig") for n in copied):
        etc = app / "Contents/Resources/etc/fonts"
        etc.mkdir(parents=True, exist_ok=True)
        (etc / "fonts.conf").write_text(
            """<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <dir>/System/Library/Fonts</dir>
  <dir>/Library/Fonts</dir>
  <dir>~/Library/Fonts</dir>
  <cachedir>~/Library/Caches/edu.asu.transmitia/fontconfig</cachedir>
</fontconfig>
"""
        )

    # Firmar (ad hoc) cada binario modificado y luego el paquete completo.
    # (el ejecutable principal se firma junto con el paquete en bundle-macos.sh)
    for dst, _src, _rp in targets:
        if dst == exe:
            continue
        identity = os.environ.get("APPLE_SIGNING_IDENTITY") or "-"
        subprocess.run(["codesign", "--force", "--options", "runtime", "--timestamp=none" if identity == "-" else "--timestamp",
                        "--sign", identity, str(dst)], check=True, capture_output=True)
    print(f"plugins: {len(PLUGINS)}  librerías: {len(copied)}")


if __name__ == "__main__":
    main()
