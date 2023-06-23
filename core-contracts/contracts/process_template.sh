#!/bin/bash

set -euo pipefail

if (( $# < 3 )); then
    echo "Usage: $0 <input-directory> <output-directory> <yaml-files...>"
    exit 1
fi

in_dir="$1"
shift 1

out_dir="$1"
shift 1

# The rest of the args will be treated as YAML files
yaml_files=("$@")
shift $#

# Create tmpfile for YAML, and remove it when script exits
yaml_tmp=$(mktemp --suffix .yaml)
trap 'rm -f -- "$yaml_tmp"' INT TERM HUP EXIT

# Combine all YAML
echo "Using YAML files: ${yaml_files[*]}"
cat "${yaml_files[@]}" > "$yaml_tmp"

# Get list of files, null separated
file_list=$(find "$in_dir" -type f -printf '%P\n')

# Copy in_dir to out_dir, while processing template files
for file in $file_list; do
    src_path="$in_dir/$file"
    dst_path="$out_dir/$file"
    mkdir -p "$(dirname "$dst_path")"
    case $file in
        # If file ends in ".mustache" run it through template engine
        *.mustache)
            # Strip suffix
            dst_path=${dst_path%.mustache}
            echo "Process template: $src_path -> $dst_path"
            mustache "$yaml_tmp" "$src_path" > "$dst_path"
            ;;
        # Else just copy
        *)
            echo "Copy file: $src_path -> $dst_path"
            cp -a "$src_path" "$dst_path"
            ;;
    esac
done