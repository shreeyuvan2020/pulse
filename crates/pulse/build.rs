// Gives pulse.exe real Windows version info (product name and version).
// SignPath enforces those fields when it signs a release, and Explorer
// shows them in the file's Properties.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "Pulse");
        res.set(
            "FileDescription",
            "Pulse - passive time tracking for building with AI coding agents",
        );
        res.set("CompanyName", "Hack Club");
        res.set("LegalCopyright", "AGPL-3.0-only");
        res.set("OriginalFilename", "pulse.exe");
        res.compile().expect("embedding Windows version info");
    }
}
