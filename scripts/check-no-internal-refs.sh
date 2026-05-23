#!/usr/bin/env bash
set -euo pipefail

# Scan tracked project files for private deployment references before they
# reach the public repository. The regex is assembled so this checker does not
# match itself when the whole tree is scanned.
forbidden_regex='a[s]gard|s[e]ntinel|carlsen[.]io|10[.]168|10[.]88|cluster'

is_allowlisted_match() {
    local file="$1"
    local line="$2"
    local remaining="$line"

    if [[ "$file" == "crates/agent/src/display.rs" ]]; then
        # WHY: conventional marker-file variable/test wording, not an external service.
        case "$line" in
            *"let sentinel = format!"*|\
            *"std::path::Path::new(&sentinel)"*|\
            *"Sentinel written last"*|\
            *"fs::write(&sentinel"*|\
            *"persistent_config_creates_sentinel_and_version"*|\
            *"Verify sentinel and version"*|\
            *"Sentinel missing"*|\
            *"persistent_config_sentinel_prevents_reseeding"*|\
            *"even with a corrupt/partial config dir, the sentinel"*|\
            *"beam-test-sentinel-"*|\
            *"after sentinel exists"*|\
            *"The sentinel file marks initialization complete"*|\
            *"let sentinel = home.join"*|\
            *"assert!(sentinel.exists())"*|\
            *"Second call: sentinel exists"*)
                remaining="${remaining//sentinel/}"
                remaining="${remaining//Sentinel/}"
                ;;
        esac
    fi

    # WHY: keyboard-layout terminology for grouped navigation keys.
    if [[ "$file" == "web/src/keymap.ts" && "$line" == *"Navigation cluster"* ]]; then
        remaining="${remaining//cluster/}"
        remaining="${remaining//Cluster/}"
    fi

    if [[ "$remaining" != "$line" ]] && ! grep -Eiq "$forbidden_regex" <<<"$remaining"; then
        return 0
    fi

    return 1
}

files=()
if (($#)); then
    files=("$@")
else
    while IFS= read -r -d '' file; do
        files+=("$file")
    done < <(git ls-files -z)
fi

violations=0
for file in "${files[@]}"; do
    [[ -f "$file" ]] || continue

    # The checker intentionally contains the blocked patterns.
    [[ "$file" == "scripts/check-no-internal-refs.sh" ]] && continue

    while IFS= read -r match; do
        [[ -n "$match" ]] || continue
        line_no="${match%%:*}"
        line="${match#*:}"
        if is_allowlisted_match "$file" "$line"; then
            continue
        fi

        printf '%s:%s: private deployment reference: %s\n' "$file" "$line_no" "$line"
        violations=$((violations + 1))
    done < <(grep -nEIi "$forbidden_regex" -- "$file" || true)
done

if ((violations)); then
    printf '\nFAIL: %d private deployment reference(s) found in tracked files.\n' "$violations"
    printf 'Keep public code/docs standalone. Move deployment-specific values to runtime configuration.\n'
    exit 1
fi
