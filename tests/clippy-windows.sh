#!/bin/sh
# `cargo clippy` for Windows, from a Mac. rbx_binary needs zstd, and a Mac has no C headers for Windows
# to compile it. Clippy never links, so a compiler that does nothing is enough.
tools=$(mktemp -d)
trap 'rm -rf "$tools"' EXIT
printf '#!/bin/sh\nexit 0\n' > "$tools/cc"
cat > "$tools/ar" <<'AR'
#!/bin/sh
for arg in "$@"; do
	case "$arg" in
		-out:* | /OUT:*) : > "${arg#*:}" ;;
		*.a | *.lib) : > "$arg" ;;
	esac
done
AR
chmod +x "$tools/cc" "$tools/ar"
CC_x86_64_pc_windows_msvc="$tools/cc" AR_x86_64_pc_windows_msvc="$tools/ar" cargo clippy --target x86_64-pc-windows-msvc --all-targets "$@"
