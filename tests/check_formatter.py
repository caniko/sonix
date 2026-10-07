"""Run with the flake formatter's bin/treefmt; prove both CI formatters run."""
import pathlib
import subprocess
import sys
import tempfile

formatter = pathlib.Path(sys.argv[1]).resolve()
assert formatter.name == "treefmt", "CI discovers the formatter by this name"
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    (root / "Cargo.toml").touch()
    samples = {"sample.nix": "{hello=1;}\n", "sample.rs": "fn main(){println!(\"hello\");}\n"}
    for name, unformatted in samples.items():
        path = root / name
        path.write_text(unformatted)
        args = [str(formatter), "--no-cache", name]
        failed = subprocess.run([*args, "--ci"], cwd=root, capture_output=True, text=True)
        assert failed.returncode != 0, f"{name} escaped the CI formatting gate"
        subprocess.run(args, cwd=root, check=True)
        assert path.read_text() != unformatted, f"{name} formatter was skipped"
        subprocess.run([*args, "--ci"], cwd=root, check=True)
