#!/usr/bin/env bash
# Checks every libcurl number src/curl.rs declares against the curl headers of the machine it runs
# on (the dev container has them), so a wrong constant is a failed check, not a request that sets
# the wrong option. Run in the dev container: bash scripts/dev.sh bash scripts/check-curl-abi.sh
set -euo pipefail
src="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/src/curl.rs"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Rust's value for each name: evaluated by the same arithmetic, in Python.
names="$(grep -oE '^pub const (CURL[A-Z_]+):' "$src" | sed -E 's/pub const (.*):/\1/')"
{
	echo '#include <stdio.h>'
	echo '#include <curl/curl.h>'
	echo 'int main(void) {'
	for n in $names; do echo "  printf(\"$n %ld\\n\", (long)($n));"; done
	echo '  printf("sizeof_CURLMsg %zu\\n", sizeof(CURLMsg));'
	echo '  printf("sizeof_curl_header %zu\\n", sizeof(struct curl_header));'
	echo '  printf("sizeof_curl_sockaddr %zu\\n", sizeof(struct curl_sockaddr));'
	echo '  return 0; }'
} >"$work/abi.c"
cc -o "$work/abi" "$work/abi.c" -lcurl
"$work/abi" | sort >"$work/c.txt"

python3 - "$src" >"$work/rust.txt" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
env = {}
for name, expr in re.findall(r'^(?:pub )?const ([A-Z_]+): [^=]+= ([^;]+);', text, re.M):
	expr = re.sub(r'\b(?:0x[0-9a-fA-F_]+|[0-9][0-9_]*)\b', lambda m: m.group().replace('_', ''), expr)
	env[name] = eval(re.sub(r'\b([A-Z][A-Z0-9_]+)\b', lambda m: str(env.get(m.group(1), m.group(1))), expr))
for name, value in sorted(env.items()):
	if name.startswith('CURL'):
		print(name, value)
PY
sort -o "$work/rust.txt" "$work/rust.txt"
grep -v '^sizeof_' "$work/c.txt" >"$work/c-names.txt"
diff "$work/c-names.txt" "$work/rust.txt" && echo "all $(wc -l <"$work/rust.txt") constants match"
grep '^sizeof_' "$work/c.txt"
