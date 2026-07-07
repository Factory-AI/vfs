#!/bin/bash
#
# Simulates a workload similar to `npx create-react-app` for testing
# AgentFS/FUSE performance.
#
# Based on observed FUSE operation distribution:
#   lookup:  18090 (~52%)  - path resolution
#   create:  4085  (~12%)  - file creation
#   mkdir:   2378  (~7%)   - directory creation
#   readdir: 2610  (~7.5%) - directory listing
#   write:   2233  (~6%)   - file writes (avg 2700 bytes, max 50KB)
#   flush:   2221  (~6%)   - flush after write
#   release: 2219  (~6%)   - close file handle
#   setattr: 674   (~2%)   - chmod/permissions
#   getattr: 521   (~1.5%) - stat operations
#   open:    7     (~0%)   - open existing files
#   read:    6     (~0%)   - read operations
#
# Usage: ./simulate-npm-workload.sh [target_dir] [scale]
#   target_dir: Directory to create the simulated project (default: ./sim-project)
#   scale:      Multiplier for operation counts (default: 1.0, use 0.1 for quick test)

set -e

RANDOM=42
TARGET_DIR="${1:-./sim-project}"
SCALE="${2:-1.0}"

# Scale the operation counts
scale_int() {
    result=$(echo "scale=0; ($1 * $SCALE + 0.5) / 1" | bc)
    echo "${result:-0}"
}

NUM_DIRS=$(scale_int 2378)
NUM_FILES=$(scale_int 4085)
NUM_READDIRS=$(scale_int 2610)
NUM_SETATTRS=$(scale_int 674)
NUM_GETATTRS=$(scale_int 521)
NUM_EXTRA_LOOKUPS=$(scale_int 5000)  # Extra lookups beyond those from other ops

# Ensure minimums for very small scales
[ "$NUM_DIRS" -lt 1 ] && NUM_DIRS=1
[ "$NUM_FILES" -lt 1 ] && NUM_FILES=1

# Package names to simulate realistic npm structure
PACKAGES=(
    "react" "react-dom" "react-scripts"
    "webpack" "webpack-cli" "webpack-dev-server"
    "babel-core" "babel-loader" "babel-preset-env" "babel-preset-react"
    "eslint" "eslint-plugin-react" "eslint-config-react-app"
    "jest" "jest-cli" "jest-environment-jsdom"
    "postcss" "postcss-loader" "autoprefixer" "cssnano"
    "typescript" "ts-loader" "@types/react" "@types/node"
    "lodash" "axios" "moment" "classnames"
    "sass" "sass-loader" "css-loader" "style-loader"
    "html-webpack-plugin" "mini-css-extract-plugin"
    "terser-webpack-plugin" "optimize-css-assets-webpack-plugin"
    "dotenv" "cross-env" "rimraf" "npm-run-all"
    "chalk" "commander" "inquirer" "ora"
    "source-map" "source-map-support" "sourcemap-codec"
    "ajv" "schema-utils" "json5" "yaml"
    "@jridgewell/sourcemap-codec" "@jridgewell/trace-mapping"
    "browserslist" "caniuse-lite" "electron-to-chromium"
)

# Subdirectories commonly found in npm packages
SUBDIRS=("lib" "dist" "src" "types" "cjs" "esm" "build" "test" "tests" "__tests__")

# Generate content of varying sizes (avg ~2700 bytes, min few bytes, max ~50KB)
generate_content() {
    local size_category=$(($1 % 100))
    local content=""

    if [ $size_category -lt 10 ]; then
        # ~10%: Small files (10-100 bytes)
        local size=$((10 + $1 % 90))
        content=$(head -c $size /dev/urandom | base64 | head -c $size)
    elif [ $size_category -lt 70 ]; then
        # ~60%: Medium files (500-5000 bytes, centered around 2700)
        local size=$((500 + $1 % 4500))
        content=$(head -c $size /dev/urandom | base64 | head -c $size)
    elif [ $size_category -lt 95 ]; then
        # ~25%: Larger files (5000-20000 bytes)
        local size=$((5000 + $1 % 15000))
        content=$(head -c $size /dev/urandom | base64 | head -c $size)
    else
        # ~5%: Large files (20000-50000 bytes)
        local size=$((20000 + $1 % 30000))
        content=$(head -c $size /dev/urandom | base64 | head -c $size)
    fi

    echo "$content"
}

echo "=== NPM Workload Simulator ==="
echo "Target directory: $TARGET_DIR"
echo "Scale factor: $SCALE"
echo "Operations to perform:"
echo "  - mkdir:        $NUM_DIRS"
echo "  - create+write: $NUM_FILES"
echo "  - readdir:      $NUM_READDIRS"
echo "  - setattr:      $NUM_SETATTRS"
echo "  - getattr:      $NUM_GETATTRS"
echo "  - extra lookup: $NUM_EXTRA_LOOKUPS"
echo ""

# Clean up any existing directory
rm -rf "$TARGET_DIR"

# Track created paths for later operations
CREATED_DIRS=()
CREATED_FILES=()

# Phase 1: Create directory structure (triggers lookup + mkdir)
echo "Phase 1: Creating directories ($NUM_DIRS)..."
START_TIME=$(date +%s.%N)

mkdir -p "$TARGET_DIR/node_modules"
CREATED_DIRS+=("$TARGET_DIR" "$TARGET_DIR/node_modules")

dirs_created=2
while [ $dirs_created -lt $NUM_DIRS ]; do
    # Pick a random package
    pkg="${PACKAGES[$((RANDOM % ${#PACKAGES[@]}))]}"
    pkg_dir="$TARGET_DIR/node_modules/$pkg"

    if [ ! -d "$pkg_dir" ]; then
        mkdir -p "$pkg_dir"
        CREATED_DIRS+=("$pkg_dir")
        ((dirs_created++))
    fi

    # Create subdirectories
    for subdir in "${SUBDIRS[@]}"; do
        if [ $dirs_created -ge $NUM_DIRS ]; then
            break
        fi

        sub_path="$pkg_dir/$subdir"
        if [ ! -d "$sub_path" ]; then
            mkdir -p "$sub_path"
            CREATED_DIRS+=("$sub_path")
            ((dirs_created++))
        fi

        # Create nested node_modules (like eslint/node_modules/locate-path)
        if [ $((RANDOM % 5)) -eq 0 ] && [ $dirs_created -lt $NUM_DIRS ]; then
            nested_pkg="${PACKAGES[$((RANDOM % ${#PACKAGES[@]}))]}"
            nested_path="$pkg_dir/node_modules/$nested_pkg"
            if [ ! -d "$nested_path" ]; then
                mkdir -p "$nested_path"
                CREATED_DIRS+=("$nested_path")
                ((dirs_created++))

                # Add subdirs to nested package too
                for nested_sub in "${SUBDIRS[@]:0:3}"; do
                    if [ $dirs_created -ge $NUM_DIRS ]; then
                        break
                    fi
                    nested_sub_path="$nested_path/$nested_sub"
                    if [ ! -d "$nested_sub_path" ]; then
                        mkdir -p "$nested_sub_path"
                        CREATED_DIRS+=("$nested_sub_path")
                        ((dirs_created++))
                    fi
                done
            fi
        fi
    done
done

END_TIME=$(date +%s.%N)
MKDIR_TIME=$(echo "$END_TIME - $START_TIME" | bc)
echo "  Created ${#CREATED_DIRS[@]} directories in ${MKDIR_TIME}s"

# Phase 2: Create files with content (triggers lookup + create + write + flush + release)
echo "Phase 2: Creating files with content ($NUM_FILES)..."
echo "  (This simulates: lookup -> create -> write -> flush -> release per file)"
START_TIME=$(date +%s.%N)

# File extensions and their typical content patterns
declare -A FILE_PATTERNS
FILE_PATTERNS=(
    ["index.js"]="module.exports"
    ["index.mjs"]="export default"
    ["index.cjs"]="module.exports"
    ["index.d.ts"]="declare module"
    ["package.json"]='{"name":'
    ["README.md"]="# "
    ["LICENSE"]="MIT License"
    ["main.js"]="module.exports"
    ["utils.js"]="function"
    ["helpers.js"]="const"
    ["constants.js"]="module.exports"
    ["types.d.ts"]="export interface"
    ["tsconfig.json"]='{"compilerOptions":'
    [".eslintrc.js"]="module.exports"
)

FILE_NAMES=("${!FILE_PATTERNS[@]}")
total_bytes=0
files_created=0

while [ $files_created -lt $NUM_FILES ]; do
    # Pick a random directory
    dir="${CREATED_DIRS[$((RANDOM % ${#CREATED_DIRS[@]}))]}"

    # Pick a random filename
    filename="${FILE_NAMES[$((RANDOM % ${#FILE_NAMES[@]}))]}"
    filepath="$dir/$filename"

    # Generate unique filename if exists
    if [ -f "$filepath" ]; then
        filepath="$dir/${RANDOM}_$filename"
    fi

    # Generate content with realistic size distribution
    seed=$RANDOM
    content=$(generate_content $seed)
    content_size=${#content}

    # Write file (this triggers: create -> write -> flush -> release)
    echo "$content" > "$filepath"

    CREATED_FILES+=("$filepath")
    total_bytes=$((total_bytes + content_size))
    files_created=$((files_created + 1))

    # Progress indicator every 500 files
    if [ $((files_created % 500)) -eq 0 ]; then
        echo "    $files_created files created..."
    fi
done

END_TIME=$(date +%s.%N)
CREATE_TIME=$(echo "$END_TIME - $START_TIME" | bc)
avg_bytes=$((total_bytes / files_created))
echo "  Created ${#CREATED_FILES[@]} files in ${CREATE_TIME}s"
echo "  Total bytes: $total_bytes, Average: $avg_bytes bytes/file"

# Combine all paths
ALL_PATHS=("${CREATED_FILES[@]}")

# Phase 3: setattr operations (chmod - triggers lookup + setattr)
echo "Phase 3: setattr/chmod operations ($NUM_SETATTRS)..."
START_TIME=$(date +%s.%N)

for ((i=0; i<NUM_SETATTRS; i++)); do
    path="${ALL_PATHS[$((RANDOM % ${#ALL_PATHS[@]}))]}"
    chmod 644 "$path" 2>/dev/null || chmod 755 "$path" 2>/dev/null || true
done

END_TIME=$(date +%s.%N)
SETATTR_TIME=$(echo "$END_TIME - $START_TIME" | bc)
echo "  Completed in ${SETATTR_TIME}s"

# Phase 4: getattr operations (stat - triggers lookup + getattr)
echo "Phase 4: getattr/stat operations ($NUM_GETATTRS)..."
START_TIME=$(date +%s.%N)

for ((i=0; i<NUM_GETATTRS; i++)); do
    path="${ALL_PATHS[$((RANDOM % ${#ALL_PATHS[@]}))]}"
    stat "$path" > /dev/null 2>&1 || true
done

END_TIME=$(date +%s.%N)
GETATTR_TIME=$(echo "$END_TIME - $START_TIME" | bc)
echo "  Completed in ${GETATTR_TIME}s"

# Phase 5: readdir operations (ls - triggers lookup + readdir)
echo "Phase 5: readdir operations ($NUM_READDIRS)..."
START_TIME=$(date +%s.%N)

for ((i=0; i<NUM_READDIRS; i++)); do
    dir="${CREATED_DIRS[$((RANDOM % ${#CREATED_DIRS[@]}))]}"
    ls "$dir" > /dev/null 2>&1 || true
done

END_TIME=$(date +%s.%N)
READDIR_TIME=$(echo "$END_TIME - $START_TIME" | bc)
echo "  Completed in ${READDIR_TIME}s"

# Phase 6: Extra lookup operations (access checks, path resolution)
echo "Phase 6: Extra lookup/access operations ($NUM_EXTRA_LOOKUPS)..."
START_TIME=$(date +%s.%N)

for ((i=0; i<NUM_EXTRA_LOOKUPS; i++)); do
    path="${ALL_PATHS[$((RANDOM % ${#ALL_PATHS[@]}))]}"
    # test -e triggers lookup without full stat
    test -e "$path" > /dev/null 2>&1 || true
done

END_TIME=$(date +%s.%N)
LOOKUP_TIME=$(echo "$END_TIME - $START_TIME" | bc)
echo "  Completed in ${LOOKUP_TIME}s"

# Summary
echo ""
echo "=== Summary ==="
TOTAL_TIME=$(echo "$MKDIR_TIME + $CREATE_TIME + $SETATTR_TIME + $GETATTR_TIME + $READDIR_TIME + $LOOKUP_TIME" | bc)
echo "Total time: ${TOTAL_TIME}s"
echo ""
echo "Breakdown:"
printf "  %-12s %8ss (%d ops)\n" "mkdir:" "$MKDIR_TIME" "$NUM_DIRS"
printf "  %-12s %8ss (%d ops, avg %d bytes)\n" "create+write:" "$CREATE_TIME" "$NUM_FILES" "$avg_bytes"
printf "  %-12s %8ss (%d ops)\n" "setattr:" "$SETATTR_TIME" "$NUM_SETATTRS"
printf "  %-12s %8ss (%d ops)\n" "getattr:" "$GETATTR_TIME" "$NUM_GETATTRS"
printf "  %-12s %8ss (%d ops)\n" "readdir:" "$READDIR_TIME" "$NUM_READDIRS"
printf "  %-12s %8ss (%d ops)\n" "lookup:" "$LOOKUP_TIME" "$NUM_EXTRA_LOOKUPS"
echo ""
echo "Created: ${#CREATED_DIRS[@]} directories, ${#CREATED_FILES[@]} files"
echo "Total disk usage:"
du -sh "$TARGET_DIR"
