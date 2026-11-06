"""Synthetic capture/editor checks; no screen permission or user clipboard access."""
from pathlib import Path
import platform
import subprocess
root = Path(__file__).resolve().parents[1]
out = root / 'target/capture-check'
out.mkdir(parents=True, exist_ok=True)
subprocess.run(['xcrun', 'swiftc', '-swift-version', '5', '-target', f'{platform.machine()}-apple-macosx12.0', '-D', 'CAPTURE_TEST', 'macos/CaptureCore.swift', 'macos/Capture.swift', 'tests/capture_test.swift', '-o', str(out / 'core-test')], cwd=root, check=True)
subprocess.run([str(out / 'core-test'), str(out / 'annotation.png')], cwd=root, check=True, timeout=30)
