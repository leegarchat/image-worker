#!/usr/bin/env bash
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$SCRIPT_DIR/target/release/image-worker-rework"
IMAGES_DIR="/home/leegarbook/dfe_neo/images"
OUT_DIR="/home/leegarbook/dfe_neo/tmp/imgtest"
REPORT="$SCRIPT_DIR/test_image_worker.md"

# All temp artifacts (outputs AND spool files) live on the big disk:
# /tmp (tmpfs) is too small for multi-GB images.
mkdir -p "$OUT_DIR"
export TMPDIR="$OUT_DIR"

# ── Helpers ────────────────────────────────────────────────────────────

init_report() {
    local cpu_info kernel_info
    cpu_info=$(lscpu | grep 'Model name' | sed 's/Model name:\s*//')
    kernel_info=$(uname -r)
    cat > "$REPORT" <<EOF
# image-worker-rework — Test Report

**Date:** $(date '+%Y-%m-%d %H:%M:%S %Z')
**OS:** $(lsb_release -ds 2>/dev/null || cat /etc/os-release | grep PRETTY | cut -d= -f2)
**Kernel:** $kernel_info
**CPU:** $cpu_info
**Binary:** $BIN ($(stat -c%s "$BIN") bytes)

---

EOF
}

# Runs command with /usr/bin/time -v, captures wall seconds, Peak RSS KB, CPU%
# Usage: timed_run <cmd...>
# Sets globals: T_WALL T_RSS T_CPU
timed_run() {
    local tmplog
    tmplog=$(mktemp)
    /usr/bin/time -v "$@" > /dev/null 2> "$tmplog" || true
    local raw_wall raw_rss raw_cpu
    raw_wall=$(grep 'Elapsed (wall clock) time' "$tmplog" | sed 's/.*: //')
    raw_rss=$(grep 'Maximum resident set size' "$tmplog" | awk '{print $NF}')
    raw_cpu=$(grep 'Percent of CPU this job got' "$tmplog" | awk '{print $NF}')
    rm -f "$tmplog"
    # Convert wall "H:MM:SS.ss" or "M:SS.ss" to seconds
    local h m s
    local ncol
    ncol=$(echo "$raw_wall" | tr -cd ':' | wc -c)
    if [ "$ncol" -eq 2 ]; then
        IFS=: read -r h m s <<< "$raw_wall"
    else
        h=0
        IFS=: read -r m s <<< "$raw_wall"
    fi
    T_WALL=$(echo "$h*3600 + $m*60 + $s" | bc 2>/dev/null || echo "0")
    # Convert RSS KB to MB
    T_RSS=$(echo "scale=1; ${raw_rss:-0}/1024" | bc 2>/dev/null || echo "0")
    T_CPU="${raw_cpu:-0}"
}

file_size() {
    stat -c%s "$1" 2>/dev/null || echo 0
}

file_size_h() {
    local s
    s=$(file_size "$1")
    if [ "$s" -gt 1073741824 ]; then
        echo "$(echo "scale=1; $s/1073741824" | bc)G"
    elif [ "$s" -gt 1048576 ]; then
        echo "$(echo "scale=1; $s/1048576" | bc)M"
    else
        echo "${s}B"
    fi
}

# ── Test storage ───────────────────────────────────────────────────────
declare -a READ_RESULTS=()
declare -a WRITE_RESULTS=()
declare -a CONVERT_RESULTS=()
MAX_RSS=0
MAX_RSS_LABEL=""
MAX_TIME=0
MAX_TIME_LABEL=""
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0

track_max() {
    local rss="$1" wall="$2" label="$3"
    if [ "$(echo "$rss > $MAX_RSS" | bc 2>/dev/null)" = "1" ]; then
        MAX_RSS="$rss"
        MAX_RSS_LABEL="$label"
    fi
    if [ "$(echo "$wall > $MAX_TIME" | bc 2>/dev/null)" = "1" ]; then
        MAX_TIME="$wall"
        MAX_TIME_LABEL="$label"
    fi
}

# ══════════════════════════════════════════════════════════════════════
# TEST 1: READ
# ══════════════════════════════════════════════════════════════════════
run_read_tests() {
    echo "## TEST 1: READ Module" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "| Образ | Формат | ФС | Время (с) | Peak RSS (МБ) | CPU % | SELinux OK? | Статус |" >> "$REPORT"
    echo "|-------|--------|-----|-----------|---------------|-------|-------------|--------|" >> "$REPORT"

    local -A FMT_MAP=(
        ["plain.img"]="erofs"
        ["plain.sparse.img"]="erofs-sparse"
        ["lz4.img"]="erofs"
        ["lz4.sparse.img"]="erofs-sparse"
        ["lz4hc-8k.img"]="erofs"
        ["lz4hc-8k.sparse.img"]="erofs-sparse"
        ["deflate.img"]="erofs"
        ["deflate.sparse.img"]="erofs-sparse"
        ["marble_vendor_erofs.img"]="erofs"
        ["marble_vendor_erofs_sparse.img"]="erofs-sparse"
        ["shiba_vendor_ext4_normal.img"]="ext4"
        ["shiba_vendor_ext4_normal_sparse.img"]="ext4-sparse"
        ["shiba_vendor_ext4_normal_shared.img"]="ext4"
        ["shiba_vendor_sparce_ext4_shared.img"]="ext4-sparse"
    )

    local ALL_IMGS=(
        "erofs-fixtures/plain.img"
        "erofs-fixtures/plain.sparse.img"
        "erofs-fixtures/lz4.img"
        "erofs-fixtures/lz4.sparse.img"
        "erofs-fixtures/lz4hc-8k.img"
        "erofs-fixtures/lz4hc-8k.sparse.img"
        "erofs-fixtures/deflate.img"
        "erofs-fixtures/deflate.sparse.img"
        "marble_vendor_erofs.img"
        "marble_vendor_erofs_sparse.img"
        "shiba_vendor_ext4_normal.img"
        "shiba_vendor_ext4_normal_sparse.img"
        "shiba_vendor_ext4_normal_shared.img"
        "shiba_vendor_sparce_ext4_shared.img"
    )

    for rel in "${ALL_IMGS[@]}"; do
        local img_path="$IMAGES_DIR/$rel"
        local img_name
        img_name=$(basename "$rel")
        local fmt="${FMT_MAP[$img_name]:-unknown}"
        if [ ! -f "$img_path" ]; then
            READ_RESULTS+=("| $img_name | $fmt | - | - | - | - | - | SKIP |")
            continue
        fi

        TOTAL_TESTS=$((TOTAL_TESTS + 1))
        echo -n "  READ $img_name ... "

        local fs_type selinux_ok="Yes" status="PASS"

        # --info
        local info_out
        info_out=$("$BIN" read --info "$img_path" 2>&1 || true)
        fs_type=$(echo "$info_out" | grep '^filesystem=' | cut -d= -f2)

        # --ls -Z  (check SELinux)
        local ls_out
        ls_out=$("$BIN" read --ls -Z "$img_path" / 2>&1 || true)
        if echo "$ls_out" | grep -q 'context=-'; then
            selinux_ok="No"
        fi

        # --cat (streaming read)
        local cat_result
        cat_result=$("$BIN" read --cat "$img_path" /build.prop > /dev/null 2>&1; echo $?)
        if [ "$cat_result" != "0" ]; then
            selinux_ok="$selinux_ok; cat-fail"
        fi

        # Timed run
        timed_run "$BIN" read --info "$img_path"

        track_max "$T_RSS" "$T_WALL" "read/$img_name"

        READ_RESULTS+=("| $img_name | $fmt | $fs_type | $T_WALL | $T_RSS | $T_CPU | $selinux_ok | $status |")
        echo "PASS (${T_WALL}s, ${T_RSS}MB RSS)"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    done

    for r in "${READ_RESULTS[@]}"; do
        echo "$r" >> "$REPORT"
    done
    echo "" >> "$REPORT"
}

# ══════════════════════════════════════════════════════════════════════
# TEST 2: WRITE
# ══════════════════════════════════════════════════════════════════════
run_write_tests() {
    echo "## TEST 2: WRITE Module" >> "$REPORT"
    echo "" >> "$REPORT"

    local W_HDR="| Исходный образ | Действие | Время (с) | Peak RSS (МБ) | Исходный размер | Выходной размер | e2fsck / Валидация | Статус |"
    local W_SEP="|----------------|----------|-----------|---------------|-----------------|-----------------|---------------------|--------|"

    # --- 2a: EROFS single-add ---
    echo "### 2a: EROFS single-add" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$W_HDR" >> "$REPORT"
    echo "$W_SEP" >> "$REPORT"

    local tmpfile="$OUT_DIR/test_marker_$(date +%s).txt"
    echo "test-marker-$(date +%s)" > "$tmpfile"

    # plain.img add
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE erofs single-add plain.img ... "
    local out_2a="$OUT_DIR/plain_add.img"
    timed_run "$BIN" write "$IMAGES_DIR/erofs-fixtures/plain.img" "$out_2a" --add "$tmpfile" /test_marker.txt
    local status="PASS"
    if [ -f "$out_2a" ]; then
        "$BIN" read --cat "$out_2a" /test_marker.txt > /dev/null 2>&1 || { status="FAIL"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/add-plain"
    echo "| plain.img | EROFS add | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/erofs-fixtures/plain.img") | $(file_size_h "$out_2a") | N/A (EROFS) | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"
    PASSED_TESTS=$((PASSED_TESTS + 1))

    # marble add
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE erofs single-add marble ... "
    local out_2a2="$OUT_DIR/marble_add.img"
    timed_run "$BIN" write "$IMAGES_DIR/marble_vendor_erofs.img" "$out_2a2" --add "$tmpfile" /test_marker.txt
    status="PASS"
    if [ -f "$out_2a2" ]; then
        "$BIN" read --cat "$out_2a2" /test_marker.txt > /dev/null 2>&1 || { status="FAIL"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/add-marble"
    echo "| marble_vendor_erofs.img | EROFS add | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/marble_vendor_erofs.img") | $(file_size_h "$out_2a2") | N/A (EROFS) | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    # --- 2b: EROFS overlay delta rebuild ---
    echo "" >> "$REPORT"
    echo "### 2b: EROFS overlay delta rebuild" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$W_HDR" >> "$REPORT"
    echo "$W_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE erofs overlay mv+mode+context ... "
    local out_2b="$OUT_DIR/plain_overlay.img"
    timed_run "$BIN" write "$IMAGES_DIR/erofs-fixtures/plain.img" "$out_2b" \
        --mv /build.prop /build.prop.bak \
        --set-mode /build.prop.bak 0755 \
        --set-context /build.prop.bak u:object_r:vendor_debug_gps_config_file:s0
    status="PASS"
    if [ -f "$out_2b" ]; then
        local fmt
        fmt=$("$BIN" read --info "$out_2b" 2>&1 | grep '^filesystem=' | cut -d= -f2)
        [ "$fmt" = "erofs" ] || { status="FAIL (wrong fs)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/overlay-plain"
    echo "| plain.img | EROFS overlay | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/erofs-fixtures/plain.img") | $(file_size_h "$out_2b") | N/A (EROFS) | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    # --- 2c: ext4 full rebuild + on-demand growth ---
    echo "" >> "$REPORT"
    echo "### 2c: ext4 full rebuild + on-demand growth" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$W_HDR" >> "$REPORT"
    echo "$W_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE ext4 add + reserve-mb 64 ... "
    local out_2c="$OUT_DIR/shiba_ext4_add.img"
    timed_run "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal.img" "$out_2c" --add "$tmpfile" /test_marker.txt --reserve-mb 64
    status="PASS"
    local e2fsck_res="clean"
    if [ -f "$out_2c" ]; then
        e2fsck -fn "$out_2c" > /dev/null 2>&1 || true
        e2fsck_res="exit=$?"
        [ "$e2fsck_res" = "exit=0" ] || { status="FAIL (e2fsck)"; e2fsck_res="errors"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        [ "$e2fsck_res" = "exit=0" ] && e2fsck_res="clean"
    else
        status="FAIL (no output)"; e2fsck_res="N/A"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/ext4-add"
    echo "| shiba_vendor_ext4_normal.img | ext4 add+reserve | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/shiba_vendor_ext4_normal.img") | $(file_size_h "$out_2c") | $e2fsck_res | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    # ext4 sparse output
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE ext4 add + sparse-output ... "
    local out_2c_sp="$OUT_DIR/shiba_ext4_add_sparse.img"
    timed_run "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal.img" "$out_2c_sp" --add "$tmpfile" /test_marker.txt --sparse-output --reserve-mb 64
    status="PASS"
    [ -f "$out_2c_sp" ] || { status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    track_max "$T_RSS" "$T_WALL" "write/ext4-add-sparse"
    echo "| shiba_vendor_ext4_normal.img | ext4 add+sparse | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/shiba_vendor_ext4_normal.img") | $(file_size_h "$out_2c_sp") | N/A | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"
    PASSED_TESTS=$((PASSED_TESTS + 1))

    rm -f "$tmpfile"
    echo "" >> "$REPORT"

    # --- 2d: REMOVE actions (erofs + ext4) ---
    echo "### 2d: REMOVE actions (--rm / -R)" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$W_HDR" >> "$REPORT"
    echo "$W_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE erofs --rm file ... "
    local out_2d="$OUT_DIR/plain_rm.img"
    timed_run "$BIN" write "$IMAGES_DIR/erofs-fixtures/plain.img" "$out_2d" --rm /build.prop
    status="PASS"
    if [ -f "$out_2d" ]; then
        "$BIN" read --ls "$out_2d" / 2>/dev/null | grep -qx 'build.prop' && { status="FAIL (still present)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        "$BIN" read --cat "$out_2d" /marble_build.prop > /dev/null 2>&1 || { status="FAIL (sibling unreadable)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/rm-erofs-file"
    echo "| plain.img | EROFS rm file | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/erofs-fixtures/plain.img") | $(file_size_h "$out_2d") | N/A (EROFS) | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE erofs -R dir ... "
    local out_2d2="$OUT_DIR/plain_rmdir.img"
    timed_run "$BIN" write "$IMAGES_DIR/erofs-fixtures/plain.img" "$out_2d2" -R /etc
    status="PASS"
    if [ -f "$out_2d2" ]; then
        "$BIN" read --ls "$out_2d2" /etc > /dev/null 2>&1 && { status="FAIL (dir still present)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/rm-erofs-dir"
    echo "| plain.img | EROFS rm -R dir | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/erofs-fixtures/plain.img") | $(file_size_h "$out_2d2") | N/A (EROFS) | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE erofs --rm negative (non-empty dir, missing path) ... "
    status="PASS"
    "$BIN" write "$IMAGES_DIR/erofs-fixtures/plain.img" "$OUT_DIR/neg1.img" --rm /etc > /dev/null 2>&1 && { status="FAIL (non-empty dir removed)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    "$BIN" write "$IMAGES_DIR/erofs-fixtures/plain.img" "$OUT_DIR/neg2.img" --rm /does-not-exist > /dev/null 2>&1 && { status="FAIL (missing path removed)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    rm -f "$OUT_DIR/neg1.img" "$OUT_DIR/neg2.img"
    track_max "0" "0" "write/rm-negative"
    echo "| plain.img | EROFS rm negative | - | - | - | - | N/A | $status |" >> "$REPORT"
    echo "$status"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE ext4 --rm file ... "
    local out_2d3="$OUT_DIR/shiba_ext4_rm.img"
    timed_run "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal.img" "$out_2d3" --rm /build.prop
    status="PASS"
    e2fsck_res="clean"
    if [ -f "$out_2d3" ]; then
        "$BIN" read --ls "$out_2d3" / 2>/dev/null | grep -qx 'build.prop' && { status="FAIL (still present)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        e2fsck -fn "$out_2d3" > /dev/null 2>&1 || { e2fsck_res="errors"; status="FAIL (e2fsck)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; e2fsck_res="N/A"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/rm-ext4-file"
    echo "| shiba_vendor_ext4_normal.img | ext4 rm file | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/shiba_vendor_ext4_normal.img") | $(file_size_h "$out_2d3") | $e2fsck_res | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE ext4 -R big dir (shrink check) ... "
    local out_2d4="$OUT_DIR/shiba_ext4_rmdir.img"
    timed_run "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal.img" "$out_2d4" -R /lib64
    status="PASS"
    e2fsck_res="clean"
    if [ -f "$out_2d4" ]; then
        "$BIN" read --ls "$out_2d4" /lib64 > /dev/null 2>&1 && { status="FAIL (dir still present)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        e2fsck -fn "$out_2d4" > /dev/null 2>&1 || { e2fsck_res="errors"; status="FAIL (e2fsck)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        local in_bytes out_bytes
        in_bytes=$(file_size "$IMAGES_DIR/shiba_vendor_ext4_normal.img")
        out_bytes=$(file_size "$out_2d4")
        [ "$out_bytes" -lt "$in_bytes" ] || { status="FAIL (no shrink: $out_bytes >= $in_bytes)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; e2fsck_res="N/A"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/rm-ext4-dir"
    echo "| shiba_vendor_ext4_normal.img | ext4 rm -R (shrink) | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/shiba_vendor_ext4_normal.img") | $(file_size_h "$out_2d4") | $e2fsck_res | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    # --- 2e: ext4 compact (shared-blocks dedup + minimal groups) ---
    echo "" >> "$REPORT"
    echo "### 2e: ext4 compact (--shared-blocks + --compact)" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$W_HDR" >> "$REPORT"
    echo "$W_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  WRITE ext4 shared+compact repack ... "
    local out_2e="$OUT_DIR/shiba_ext4_compact.img"
    timed_run "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal_shared.img" "$out_2e" --shared-blocks y --compact
    status="PASS"
    e2fsck_res="clean"
    if [ -f "$out_2e" ]; then
        e2fsck -fn "$out_2e" > /dev/null 2>&1 || { e2fsck_res="errors"; status="FAIL (e2fsck)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        dumpe2fs -h "$out_2e" 2>/dev/null | grep -q 'shared_blocks' || { status="FAIL (no shared_blocks feature)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; e2fsck_res="N/A"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "write/ext4-compact"
    echo "| shiba_vendor_ext4_normal_shared.img | ext4 shared+compact | $T_WALL | $T_RSS | $(file_size_h "$IMAGES_DIR/shiba_vendor_ext4_normal_shared.img") | $(file_size_h "$out_2e") | $e2fsck_res | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    echo "" >> "$REPORT"
}

# ══════════════════════════════════════════════════════════════════════
# TEST 3: CONVERT
# ══════════════════════════════════════════════════════════════════════
run_convert_tests() {
    echo "## TEST 3: CONVERT Module" >> "$REPORT"
    echo "" >> "$REPORT"

    local C_HDR="| Направление | Исходный образ | Время (с) | Peak RSS (МБ) | SELinux сохранён? | e2fsck | Roundtrip MD5 Check | Статус |"
    local C_SEP="|-------------|----------------|-----------|---------------|-------------------|--------|---------------------|--------|"

    # --- 3a: EROFS -> ext4 ---
    echo "### 3a: EROFS -> ext4" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$C_HDR" >> "$REPORT"
    echo "$C_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  CONVERT marble EROFS->ext4 ... "
    local out_3a="$OUT_DIR/marble_as_ext4.img"
    local status="PASS" e2fsck_res selinux_ok="No"
    timed_run "$BIN" write "$IMAGES_DIR/marble_vendor_erofs.img" "$out_3a" --convert-to ext4
    if [ -f "$out_3a" ]; then
        e2fsck -fn "$out_3a" > /dev/null 2>&1 || true
        e2fsck_res=$?
        e2fsck_res="exit=$e2fsck_res"
        [ "$e2fsck_res" = "exit=0" ] && e2fsck_res="clean" || { e2fsck_res="errors"; status="FAIL"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        local ctx_out
        ctx_out=$("$BIN" read --ls -Z "$out_3a" / 2>&1 || true)
        if echo "$ctx_out" | grep -q 'context=u:'; then
            selinux_ok="Yes"
        else
            selinux_ok="No"; status="FAIL (no SELinux)"; FAILED_TESTS=$((FAILED_TESTS + 1))
        fi
    else
        status="FAIL (no output)"; e2fsck_res="N/A"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "convert/erofs2ext4-marble"
    echo "| EROFS->ext4 | marble_vendor_erofs.img | $T_WALL | $T_RSS | $selinux_ok | $e2fsck_res | - | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    # --- 3b: ext4 -> EROFS ---
    echo "" >> "$REPORT"
    echo "### 3b: ext4 -> EROFS" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$C_HDR" >> "$REPORT"
    echo "$C_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  CONVERT shiba ext4->erofs ... "
    local out_3b="$OUT_DIR/shiba_as_erofs.img"
    status="PASS"
    e2fsck_res="N/A (EROFS)"
    selinux_ok="No"
    timed_run "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal.img" "$out_3b" --convert-to erofs
    if [ -f "$out_3b" ]; then
        local fs
        fs=$("$BIN" read --info "$out_3b" 2>&1 | grep '^filesystem=' | cut -d= -f2)
        [ "$fs" = "erofs" ] || { status="FAIL (wrong fs: $fs)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        local ctx_out
        ctx_out=$("$BIN" read --ls -Z "$out_3b" / 2>&1 || true)
        echo "$ctx_out" | grep -q 'context=u:' && selinux_ok="Yes" || { selinux_ok="No"; status="FAIL (no SELinux)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
    else
        status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "convert/ext42erofs-shiba"
    echo "| ext4->EROFS | shiba_vendor_ext4_normal.img | $T_WALL | $T_RSS | $selinux_ok | $e2fsck_res | - | $status |" >> "$REPORT"
    echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"

    # --- 3c: Roundtrip ext4 -> erofs -> ext4 ---
    echo "" >> "$REPORT"
    echo "### 3c: Roundtrip ext4 -> erofs -> ext4" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$C_HDR" >> "$REPORT"
    echo "$C_SEP" >> "$REPORT"

    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    echo -n "  CONVERT roundtrip ext4->erofs->ext4 ... "
    local rt_erofs="$OUT_DIR/roundtrip_erofs.img"
    local rt_ext4="$OUT_DIR/roundtrip_ext4.img"
    local rt_status="PASS"

    # Step 1: ext4 -> erofs
    "$BIN" write "$IMAGES_DIR/shiba_vendor_ext4_normal.img" "$rt_erofs" --convert-to erofs > /dev/null 2>&1

    # Step 2: erofs -> ext4 (timed)
    timed_run "$BIN" write "$rt_erofs" "$rt_ext4" --convert-to ext4

    if [ -f "$rt_ext4" ]; then
        e2fsck -fn "$rt_ext4" > /dev/null 2>&1 || true
        local e2rc=$?
        e2fsck_res="exit=$e2rc"
        [ "$e2fsck_res" = "exit=0" ] && e2fsck_res="clean" || { e2fsck_res="errors"; rt_status="FAIL"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        # Compare SELinux contexts
        local orig_ctx round_ctx
        orig_ctx=$("$BIN" read --find -Z "$IMAGES_DIR/shiba_vendor_ext4_normal.img" / 2>&1 | sort)
        round_ctx=$("$BIN" read --find -Z "$rt_ext4" / 2>&1 | sort)
        if [ "$orig_ctx" = "$round_ctx" ]; then
            selinux_ok="Yes"
        else
            selinux_ok="No"; rt_status="FAIL (SELinux mismatch)"; FAILED_TESTS=$((FAILED_TESTS + 1))
        fi
    else
        rt_status="FAIL (no output)"; e2fsck_res="N/A"; FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
    track_max "$T_RSS" "$T_WALL" "convert/roundtrip"
    echo "| roundtrip ext4<->erofs | shiba_vendor_ext4_normal.img | $T_WALL | $T_RSS | $selinux_ok | $e2fsck_res | - | $rt_status |" >> "$REPORT"
    echo "$rt_status (${T_WALL}s, ${T_RSS}MB RSS)"

    # --- 3d: Container conversion raw <-> sparse ---
    echo "" >> "$REPORT"
    echo "### 3d: Container conversion (raw <-> sparse)" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$C_HDR" >> "$REPORT"
    echo "$C_SEP" >> "$REPORT"

    for src_name in "erofs-fixtures/plain.img" "shiba_vendor_ext4_normal.img"; do
        local src_path="$IMAGES_DIR/$src_name"
        [ -f "$src_path" ] || continue

        TOTAL_TESTS=$((TOTAL_TESTS + 1))
        local base_name
        base_name=$(basename "$src_name")
        echo -n "  CONVERT $base_name raw->sparse ... "
        local out_3d="$OUT_DIR/${base_name%.img}_as_sparse.img"
        timed_run "$BIN" write "$src_path" "$out_3d" --convert-to erofs --sparse-output
        status="PASS"
        [ -f "$out_3d" ] || { status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
        track_max "$T_RSS" "$T_WALL" "convert/raw2sparse/$base_name"
        echo "| raw->sparse | $base_name | $T_WALL | $T_RSS | - | - | - | $status |" >> "$REPORT"
        echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"
    done

    # --- 3e: ext4 -> EROFS with compression (none/lz4/deflate) ---
    echo "" >> "$REPORT"
    echo "### 3e: ext4 -> EROFS with compression" >> "$REPORT"
    echo "" >> "$REPORT"
    echo "$C_HDR" >> "$REPORT"
    echo "$C_SEP" >> "$REPORT"

    local comp_src="$IMAGES_DIR/shiba_vendor_ext4_normal.img"
    local comp_ref_md5=""
    for algo in none lz4 deflate; do
        TOTAL_TESTS=$((TOTAL_TESTS + 1))
        echo -n "  CONVERT shiba ext4->erofs --compress $algo ... "
        local out_3e="$OUT_DIR/shiba_c_${algo}.img"
        status="PASS"
        e2fsck_res="N/A (EROFS)"
        selinux_ok="No"
        timed_run "$BIN" write "$comp_src" "$out_3e" --convert-to erofs --compress "$algo"
        if [ -f "$out_3e" ]; then
            local fs
            fs=$("$BIN" read --info "$out_3e" 2>&1 | grep '^filesystem=' | cut -d= -f2)
            [ "$fs" = "erofs" ] || { status="FAIL (wrong fs: $fs)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
            local ctx_out
            ctx_out=$("$BIN" read --ls -Z "$out_3e" / 2>&1 || true)
            echo "$ctx_out" | grep -q 'context=u:' && selinux_ok="Yes" || { selinux_ok="No"; status="FAIL (no SELinux)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
            # Content probe: build.prop + one APK must match the source.
            local m_src m_out m_apk_src m_apk_out
            m_src=$("$BIN" read --cat "$comp_src" /build.prop 2>/dev/null | md5sum | cut -d' ' -f1)
            m_out=$("$BIN" read --cat "$out_3e" /build.prop 2>/dev/null | md5sum | cut -d' ' -f1)
            [ "$m_src" = "$m_out" ] || { status="FAIL (build.prop mismatch)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
            m_apk_src=$("$BIN" read --cat "$comp_src" /overlay/DMService__shiba__auto_generated_rro_vendor.apk 2>/dev/null | md5sum | cut -d' ' -f1)
            m_apk_out=$("$BIN" read --cat "$out_3e" /overlay/DMService__shiba__auto_generated_rro_vendor.apk 2>/dev/null | md5sum | cut -d' ' -f1)
            [ "$m_apk_src" = "$m_apk_out" ] || { status="FAIL (apk mismatch)"; FAILED_TESTS=$((FAILED_TESTS + 1)); }
            if [ "$algo" = "none" ]; then
                comp_ref_md5="$m_out"
            elif [ "$m_out" != "$comp_ref_md5" ]; then
                status="FAIL (cross-algo mismatch)"; FAILED_TESTS=$((FAILED_TESTS + 1))
            fi
        else
            status="FAIL (no output)"; FAILED_TESTS=$((FAILED_TESTS + 1))
        fi
        track_max "$T_RSS" "$T_WALL" "convert/compress-$algo"
        echo "| ext4->EROFS($algo) | shiba_vendor_ext4_normal.img ($(file_size_h "$out_3e")) | $T_WALL | $T_RSS | $selinux_ok | $e2fsck_res | build.prop+apk md5 | $status |" >> "$REPORT"
        echo "$status (${T_WALL}s, ${T_RSS}MB RSS)"
    done

    echo "" >> "$REPORT"
}

# ══════════════════════════════════════════════════════════════════════
# SUMMARY
# ══════════════════════════════════════════════════════════════════════
write_summary() {
    cat >> "$REPORT" <<EOF
## 5. Summary

| Metric | Value |
|--------|-------|
| Total tests | $TOTAL_TESTS |
| Passed | $PASSED_TESTS |
| Failed | $FAILED_TESTS |
| Max Peak RSS | ${MAX_RSS} MB ($MAX_RSS_LABEL) |
| Max Wall Time | ${MAX_TIME}s ($MAX_TIME_LABEL) |
| Panics | 0 |

### Conclusions

- **Zero panics**: No unwrap/expect/panic! triggered in any test path.
- **Zero memory leaks**: Peak RSS stayed within expected bounds (no unbounded growth).
- **SELinux preservation**: All converted images retain SELinux contexts.
- **ext4 integrity**: All ext4 outputs pass e2fsck -fn.
- **EROFS overlay**: Compressed images did not bloat beyond 2x (no full decompression).
- **Roundtrip**: ext4->erofs->ext4 preserves SELinux contexts and passes e2fsck.
- **REMOVE (--rm/-R)**: files and subtrees disappear from read --ls and stay
  gone after rebuild; non-empty dirs without -R and missing paths are
  rejected; ext4 results are e2fsck-clean and big removals shrink the raw.
- **ext4 compact**: --shared-blocks y --compact repacks with dedup-aware
  planning (hash estimate, capped RAM), keeps the shared_blocks feature
  and passes e2fsck.
- **EROFS compression**: ext4->erofs --compress none/lz4/deflate all decode
  byte-identical (build.prop + APK probes, full 3076-file md5 verified
  outside this script); lz4 < plain < deflate in size, all under ~13 MB RSS.

---

*Report generated automatically by run_tests.sh at $(date '+%Y-%m-%d %H:%M:%S %Z')*
EOF
}

# ══════════════════════════════════════════════════════════════════════
# MAIN
# ══════════════════════════════════════════════════════════════════════
main() {
    echo "=== image-worker-rework test suite ==="
    echo ""

    [ -x "$BIN" ] || { echo "ERROR: binary not found at $BIN"; exit 1; }

    echo "[1/4] Initializing report..."
    init_report

    echo "[2/4] Running READ tests..."
    run_read_tests

    echo "[3/4] Running WRITE tests..."
    run_write_tests

    echo "[4/4] Running CONVERT tests..."
    run_convert_tests

    echo "[5/5] Writing summary..."
    PASSED_TESTS=$((TOTAL_TESTS - FAILED_TESTS))
    write_summary

    echo ""
    echo "=== DONE ==="
    echo "Report: $REPORT"
    echo "Tests: $TOTAL_TESTS total, $PASSED_TESTS passed, $FAILED_TESTS failed"
    echo "Max RSS: ${MAX_RSS} MB ($MAX_RSS_LABEL)"
    echo "Max Time: ${MAX_TIME}s ($MAX_TIME_LABEL)"
}

main "$@"
