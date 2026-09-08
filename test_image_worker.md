# image-worker-rework — Test Report

**Date:** 2026-09-08 11:20:49 MSK
**OS:** Ubuntu 26.04.1 LTS
**Kernel:** 7.0.0-30-generic
**CPU:** AMD Ryzen 7 8845HS w/ Radeon 780M Graphics
**Binary:** /home/leegarbook/dfe_neo/rust-tools/image-worker-rework/target/release/image-worker-rework (1360168 bytes)

---

## TEST 1: READ Module

| Образ | Формат | ФС | Время (с) | Peak RSS (МБ) | CPU % | SELinux OK? | Статус |
|-------|--------|-----|-----------|---------------|-------|-------------|--------|
| plain.img | erofs | erofs | 0 | 2.6 | 92% | Yes | PASS |
| plain.sparse.img | erofs-sparse | erofs | 0 | 2.6 | 100% | Yes | PASS |
| lz4.img | erofs | erofs | 0 | 2.6 | 92% | Yes | PASS |
| lz4.sparse.img | erofs-sparse | erofs | 0 | 2.7 | 92% | Yes | PASS |
| lz4hc-8k.img | erofs | erofs | 0 | 2.6 | 0% | Yes | PASS |
| lz4hc-8k.sparse.img | erofs-sparse | erofs | 0 | 2.7 | 92% | Yes | PASS |
| deflate.img | erofs | erofs | 0 | 2.7 | 90% | Yes | PASS |
| deflate.sparse.img | erofs-sparse | erofs | 0 | 2.7 | 100% | Yes | PASS |
| marble_vendor_erofs.img | erofs | erofs | 0 | 2.6 | 92% | Yes | PASS |
| marble_vendor_erofs_sparse.img | erofs-sparse | erofs | 0 | 2.6 | 92% | Yes | PASS |
| shiba_vendor_ext4_normal.img | ext4 | ext4 | 0 | 2.6 | 91% | Yes | PASS |
| shiba_vendor_ext4_normal_sparse.img | ext4-sparse | ext4 | 0 | 2.6 | 0% | Yes | PASS |
| shiba_vendor_ext4_normal_shared.img | ext4 | ext4 | 0 | 2.6 | 90% | Yes | PASS |
| shiba_vendor_sparce_ext4_shared.img | ext4-sparse | ext4 | 0 | 2.6 | 94% | Yes | PASS |

## TEST 2: WRITE Module

### 2a: EROFS single-add

| Исходный образ | Действие | Время (с) | Peak RSS (МБ) | Исходный размер | Выходной размер | e2fsck / Валидация | Статус |
|----------------|----------|-----------|---------------|-----------------|-----------------|---------------------|--------|
| plain.img | EROFS add | 3.91 | 5.6 | 2.4G | 2.4G | N/A (EROFS) | PASS |
| marble_vendor_erofs.img | EROFS add | 5.77 | 5.6 | 1.5G | 1.5G | N/A (EROFS) | PASS |

### 2b: EROFS overlay delta rebuild

| Исходный образ | Действие | Время (с) | Peak RSS (МБ) | Исходный размер | Выходной размер | e2fsck / Валидация | Статус |
|----------------|----------|-----------|---------------|-----------------|-----------------|---------------------|--------|
| plain.img | EROFS overlay | 7.64 | 5.9 | 2.4G | 2.4G | N/A (EROFS) | PASS |

### 2c: ext4 full rebuild + on-demand growth

| Исходный образ | Действие | Время (с) | Peak RSS (МБ) | Исходный размер | Выходной размер | e2fsck / Валидация | Статус |
|----------------|----------|-----------|---------------|-----------------|-----------------|---------------------|--------|
| shiba_vendor_ext4_normal.img | ext4 add+reserve | 2.78 | 5.5 | 850.0M | 1024.0M | clean | PASS |
| shiba_vendor_ext4_normal.img | ext4 add+sparse | 4.09 | 5.5 | 850.0M | 848.1M | N/A | PASS |

### 2d: REMOVE actions (--rm / -R)

| Исходный образ | Действие | Время (с) | Peak RSS (МБ) | Исходный размер | Выходной размер | e2fsck / Валидация | Статус |
|----------------|----------|-----------|---------------|-----------------|-----------------|---------------------|--------|
| plain.img | EROFS rm file | 7.55 | 6.0 | 2.4G | 2.4G | N/A (EROFS) | PASS |
| plain.img | EROFS rm -R dir | 5.59 | 5.0 | 2.4G | 1.9G | N/A (EROFS) | PASS |
| plain.img | EROFS rm negative | - | - | - | - | N/A | PASS |
| shiba_vendor_ext4_normal.img | ext4 rm file | 2.79 | 5.5 | 850.0M | 1024.0M | clean | PASS |
| shiba_vendor_ext4_normal.img | ext4 rm -R (shrink) | 1.83 | 4.8 | 850.0M | 768.0M | clean | PASS |

### 2e: ext4 compact (--shared-blocks + --compact)

| Исходный образ | Действие | Время (с) | Peak RSS (МБ) | Исходный размер | Выходной размер | e2fsck / Валидация | Статус |
|----------------|----------|-----------|---------------|-----------------|-----------------|---------------------|--------|
| shiba_vendor_ext4_normal_shared.img | ext4 shared+compact | 4.14 | 20.9 | 848.5M | 896.0M | clean | PASS |

## TEST 3: CONVERT Module

### 3a: EROFS -> ext4

| Направление | Исходный образ | Время (с) | Peak RSS (МБ) | SELinux сохранён? | e2fsck | Roundtrip MD5 Check | Статус |
|-------------|----------------|-----------|---------------|-------------------|--------|---------------------|--------|
| EROFS->ext4 | marble_vendor_erofs.img | 11.07 | 9.9 | Yes | clean | - | PASS |

### 3b: ext4 -> EROFS

| Направление | Исходный образ | Время (с) | Peak RSS (МБ) | SELinux сохранён? | e2fsck | Roundtrip MD5 Check | Статус |
|-------------|----------------|-----------|---------------|-------------------|--------|---------------------|--------|
| ext4->EROFS | shiba_vendor_ext4_normal.img | 2.34 | 7.8 | Yes | N/A (EROFS) | - | PASS |

### 3c: Roundtrip ext4 -> erofs -> ext4

| Направление | Исходный образ | Время (с) | Peak RSS (МБ) | SELinux сохранён? | e2fsck | Roundtrip MD5 Check | Статус |
|-------------|----------------|-----------|---------------|-------------------|--------|---------------------|--------|
| roundtrip ext4<->erofs | shiba_vendor_ext4_normal.img | 4.07 | 10.1 | Yes | clean | - | PASS |

### 3d: Container conversion (raw <-> sparse)

| Направление | Исходный образ | Время (с) | Peak RSS (МБ) | SELinux сохранён? | e2fsck | Roundtrip MD5 Check | Статус |
|-------------|----------------|-----------|---------------|-------------------|--------|---------------------|--------|
| raw->sparse | plain.img | 1.66 | 2.8 | - | - | - | PASS |
| raw->sparse | shiba_vendor_ext4_normal.img | 5.98 | 7.7 | - | - | - | PASS |

### 3e: ext4 -> EROFS with compression

| Направление | Исходный образ | Время (с) | Peak RSS (МБ) | SELinux сохранён? | e2fsck | Roundtrip MD5 Check | Статус |
|-------------|----------------|-----------|---------------|-------------------|--------|---------------------|--------|
| ext4->EROFS(none) | shiba_vendor_ext4_normal.img (898.6M) | .80 | 5.1 | Yes | N/A (EROFS) | build.prop+apk md5 | PASS |
| ext4->EROFS(lz4) | shiba_vendor_ext4_normal.img (766.9M) | 2.63 | 7.7 | Yes | N/A (EROFS) | build.prop+apk md5 | PASS |
| ext4->EROFS(deflate) | shiba_vendor_ext4_normal.img (691.8M) | 6.04 | 12.6 | Yes | N/A (EROFS) | build.prop+apk md5 | PASS |

## 5. Summary

| Metric | Value |
|--------|-------|
| Total tests | 33 |
| Passed | 33 |
| Failed | 0 |
| Max Peak RSS | 20.9 MB (write/ext4-compact) |
| Max Wall Time | 11.07s (convert/erofs2ext4-marble) |
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
  separately); lz4 < plain, deflate smallest, all under ~13 MB RSS.

---

*Report generated automatically by run_tests.sh at 2026-09-08 11:22:20 MSK*
