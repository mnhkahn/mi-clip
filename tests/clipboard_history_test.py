"""Exercise clipboard history using named pasteboards and a temporary store."""
from pathlib import Path
import platform
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory() as directory:
    binary = Path(directory) / "clipboard-history-test"
    subprocess.run([
        "xcrun", "swiftc", "-swift-version", "5", "-target", f"{platform.machine()}-apple-macosx12.0",
        "macos/ClipboardHistoryStore.swift", "macos/GitHubSocialPreview.swift", "tests/clipboard_history_test.swift", "-o", str(binary),
    ], cwd=root, check=True)
    subprocess.run([str(binary)], cwd=root, check=True, timeout=30)
