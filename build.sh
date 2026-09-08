#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

DIST_DIR="$SCRIPT_DIR/dist"
PUSH_DIR="$SCRIPT_DIR/target/push"
BIN_NAME="image-worker"

TARGET_X64="x86_64-unknown-linux-musl"
TARGET_X86="i686-unknown-linux-musl"
TARGET_ARM64="aarch64-unknown-linux-musl"
TARGET_ARM32="armv7-unknown-linux-musleabihf"

ALL_TARGETS=("$TARGET_X64" "$TARGET_X86" "$TARGET_ARM64" "$TARGET_ARM32")

# Параметры по умолчанию
METHOD="auto"         # auto | cargo | cross
SELECTED_ARCH="all"   # all | x64 | x86 | arm64 | arm32

usage() {
    cat <<EOF
Использование:
  $0 [ОПЦИИ]

Опции метода сборки:
  --cargo          Принудительно использовать локальный cargo (требуются системные gcc-линкеры)
  --cross          Принудительно использовать cross (требуется Docker или Podman)
  --auto           Автовыбор: cross (если есть контейнеры), иначе cargo (по умолчанию)

Опции выбора архитектуры:
  --arch <тип>     Собрать конкретную архитектуру:
                     all   - Все 4 платформы (по умолчанию)
                     x64   - x86_64-unknown-linux-musl
                     x86   - i686-unknown-linux-musl
                     arm64 - aarch64-unknown-linux-musl
                     arm32 - armv7-unknown-linux-musleabihf
  -h, --help       Показать это сообщение

Примеры:
  $0 --cargo --arch x64
  $0 --cross --arch arm64
  $0 --arch all

Выходы:
  dist/            Статические бинарники image-worker-linux-*
  target/push/     Push-копии {name}_{arch} (x64, x86, arm64, arm32),
                   например для: adb push target/push/image-worker_arm64 /data/local/
EOF
    exit 0
}

# Парсинг аргументов CLI
while [[ $# -gt 0 ]]; do
    case "$1" in
        --cargo)
            METHOD="cargo"
            shift
            ;;
        --cross)
            METHOD="cross"
            shift
            ;;
        --auto)
            METHOD="auto"
            shift
            ;;
        --arch)
            SELECTED_ARCH="${2:-}"
            if [[ -z "$SELECTED_ARCH" ]]; then
                echo "Ошибка: для --arch не указана архитектура"
                exit 1
            fi
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Неизвестный параметр: $1"
            usage
            ;;
    esac
done

# Определение пакетного менеджера дистрибутива
detect_pkg_manager() {
    if command -v apt-get &>/dev/null; then
        echo "apt"
    elif command -v dnf &>/dev/null; then
        echo "dnf"
    elif command -v pacman &>/dev/null; then
        echo "pacman"
    else
        echo "unknown"
    fi
}

# Рекомендации команд установки
suggest_install() {
    local missing_type="$1"
    local pm
    pm="$(detect_pkg_manager)"

    echo ""
    echo "============================================================"
    echo "Внимание: отсутствуют необходимые зависимости!"
    echo "============================================================"

    if [[ "$missing_type" == "container" ]]; then
        echo "Для сборки через '--cross' требуется установленный Podman или Docker."
        echo "Рекомендуемая команда для установки:"
        case "$pm" in
            apt)    echo "  sudo apt update && sudo apt install -y podman" ;;
            dnf)    echo "  sudo dnf install -y podman" ;;
            pacman) echo "  sudo pacman -S --needed podman" ;;
            *)      echo "  Установите Podman или Docker через ваш пакетный менеджер." ;;
        esac
        echo ""
        echo "Также убедитесь, что установлен cross: cargo install cross --git https://github.com/cross-rs/cross"
    elif [[ "$missing_type" == "cargo-tools" ]]; then
        echo "Для сборки через '--cargo' требуются кросс-линкеры и musl-тулчейны."
        echo "Рекомендуемая команда для установки:"
        case "$pm" in
            apt)
                echo "  sudo apt update && sudo apt install -y musl-tools gcc-i686-linux-gnu gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf"
                ;;
            dnf)
                echo "  sudo dnf install -y musl-gcc gcc-arm-linux-gnu gcc-aarch64-linux-gnu"
                ;;
            pacman)
                echo "  sudo pacman -S --needed musl aarch64-linux-gnu-gcc arm-linux-gnueabihf-gcc lib32-glibc"
                ;;
            *)
                echo "  Установите кросс-компиляторы для x86, aarch64, armv7 и musl-tools."
                ;;
        esac
    fi
    echo "============================================================"
    echo ""
}

# Проверка наличия контейнерного движка
has_container_engine() {
    command -v podman &>/dev/null || (command -v docker &>/dev/null && docker info &>/dev/null)
}

# Проверка наличия линкера для target при сборке cargo
check_cargo_linker() {
    local target="$1"
    local linker=""

    case "$target" in
        "$TARGET_X86")
            linker="i686-linux-gnu-gcc"
            ;;
        "$TARGET_ARM64")
            linker="aarch64-linux-gnu-gcc"
            ;;
        "$TARGET_ARM32")
            linker="arm-linux-gnueabihf-gcc"
            ;;
        "$TARGET_X64")
            return 0
            ;;
    esac

    if ! command -v "$linker" &>/dev/null; then
        echo "Ошибка: не найден кросс-линкер '$linker' для $target!"
        suggest_install "cargo-tools"
        return 1
    fi
    return 0
}

# Определение итогового сборщика
BUILDER=""
if [[ "$METHOD" == "cross" ]]; then
    if ! command -v cross &>/dev/null || ! has_container_engine; then
        suggest_install "container"
        exit 1
    fi
    BUILDER="cross"
elif [[ "$METHOD" == "cargo" ]]; then
    BUILDER="cargo"
else
    # auto
    if command -v cross &>/dev/null && has_container_engine; then
        BUILDER="cross"
    else
        BUILDER="cargo"
    fi
fi

echo "==> Режим сборки: $BUILDER (выбран по правилу: $METHOD)"

# Подготовка rustup-таргетов для cargo
if [[ "$BUILDER" == "cargo" ]]; then
    echo "==> Проверка таргетов rustup..."
    rustup target add "${ALL_TARGETS[@]}" >/dev/null 2>&1 || true
fi

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

build_target() {
    local target="$1"
    local output_name="$2"
    local short_arch="$3"

    echo ""
    echo "------------------------------------------------------------"
    echo "Сборка [$output_name] -> $target"
    echo "------------------------------------------------------------"

    if [[ "$BUILDER" == "cross" ]]; then
        cross build --release --target "$target"
    else
        if ! check_cargo_linker "$target"; then
            exit 1
        fi
        case "$target" in
            "$TARGET_ARM64")
                export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="aarch64-linux-gnu-gcc"
                ;;
            "$TARGET_ARM32")
                export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="arm-linux-gnueabihf-gcc"
                ;;
            "$TARGET_X86")
                export CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER="i686-linux-gnu-gcc"
                ;;
            "$TARGET_X64")
                if command -v musl-gcc &>/dev/null; then
                    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="musl-gcc"
                fi
                ;;
        esac

        cargo build --release --target "$target"
    fi

    local src_bin="$SCRIPT_DIR/target/$target/release/$BIN_NAME"
    local dst_bin="$DIST_DIR/${BIN_NAME}-${output_name}"

    if [[ -f "$src_bin" ]]; then
        cp "$src_bin" "$dst_bin"

        # Стриппинг отладочной информации
        if command -v strip &>/dev/null; then
            strip "$dst_bin" 2>/dev/null || true
        fi

        local size
        size=$(stat -c%s "$dst_bin" 2>/dev/null || stat -f%z "$dst_bin")
        echo "Успешно: $dst_bin ($size байт)"

        # Пост-процесс: push-копия target/push/{name}_{arch} для adb push
        # (например target/push/image-worker_arm64 -> /data/local).
        # target/ в gitignore; push/ переживает запуски
        # (перезаписывается только собранная архитектура).
        mkdir -p "$PUSH_DIR"
        cp "$dst_bin" "$PUSH_DIR/${BIN_NAME}_${short_arch}"
        echo "Push-копия: $PUSH_DIR/${BIN_NAME}_${short_arch}"
    else
        echo "Ошибка: скомпилированный файл не найден: $src_bin"
        exit 1
    fi
}

# Выполнение задач сборки (3-й аргумент = короткий arch для target/push/{name}_{arch})
case "$SELECTED_ARCH" in
    all)
        build_target "$TARGET_X64"   "linux-x86_64" "x64"
        build_target "$TARGET_X86"   "linux-x86"    "x86"
        build_target "$TARGET_ARM64" "linux-arm64"  "arm64"
        build_target "$TARGET_ARM32" "linux-arm32"  "arm32"
        ;;
    x64)
        build_target "$TARGET_X64"   "linux-x86_64" "x64"
        ;;
    x86)
        build_target "$TARGET_X86"   "linux-x86"    "x86"
        ;;
    arm64)
        build_target "$TARGET_ARM64" "linux-arm64"  "arm64"
        ;;
    arm32)
        build_target "$TARGET_ARM32" "linux-arm32"  "arm32"
        ;;
    *)
        echo "Ошибка: неизвестная архитектура '$SELECTED_ARCH'"
        usage
        ;;
esac

echo ""
echo "============================================================"
echo "Готово! Сгенерированные бинарники в dist/:"
ls -lh "$DIST_DIR"
echo "Push-копии в target/push/ ({name}_{arch} для adb push):"
ls -lh "$PUSH_DIR"
echo "============================================================"
