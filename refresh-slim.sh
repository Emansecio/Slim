#!/bin/sh
# Build release do Slim e atualiza o binario implantado no PATH (macOS/Linux).
#
# O comando `slim` aponta para ~/.local/bin/slim (ou $SLIM_INSTALL_DIR/slim),
# uma COPIA ESTATICA do binario, que NAO acompanha o codigo automaticamente.
# Equivalente Unix de refresh-slim.ps1:
#
#   ./refresh-slim.sh            build release + instalacao + smoke test
#   ./refresh-slim.sh --test     idem, rodando cargo test --workspace antes
#   ./refresh-slim.sh --fast     build local mais rapido (sem LTO, 8 codegen units)
#   ./refresh-slim.sh --keep     mantem target/ apos instalar (rebuild incremental)
#
# Por padrao o script economiza disco: sem cache incremental, sem debuginfo nos
# testes e, ao final, `cargo clean` remove target/ (o binario instalado fica).
set -eu

root=$(cd "$(dirname "$0")" && pwd)
install_dir=${SLIM_INSTALL_DIR:-"$HOME/.local/bin"}
deploy="$install_dir/slim"
built="$root/target/release/slim"
run_tests=0
fast=0
keep=0
for argument in "$@"; do
    case "$argument" in
        --test) run_tests=1 ;;
        --fast) fast=1 ;;
        --keep) keep=1 ;;
        *) echo "opcao desconhecida: $argument" >&2; exit 2 ;;
    esac
done

command -v cargo >/dev/null 2>&1 || { echo 'cargo nao encontrado no PATH.' >&2; exit 1; }
command -v git >/dev/null 2>&1 || { echo 'git nao encontrado no PATH.' >&2; exit 1; }

commit=$(git -C "$root" rev-parse --verify HEAD) || {
    echo 'nao foi possivel identificar o commit HEAD para o build.' >&2
    exit 1
}
if [ -n "$(git -C "$root" status --porcelain --untracked-files=all)" ]; then
    commit="$commit-dirty"
fi

# .cargo/config.toml fixa jobs=1 para o host Windows; aqui metade dos nucleos
# basta e limita a memoria de pico. Um CARGO_BUILD_JOBS explicito prevalece.
cores=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 2)
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-$(( cores > 1 ? cores / 2 : 1 ))}
export CARGO_INCREMENTAL=0

if [ "$run_tests" = 1 ]; then
    (cd "$root" && cargo --config profile.test.debug=0 \
        --config 'profile.dev.package."*".debug=0' \
        --config profile.test.package.slim-core.debug=0 \
        --config profile.test.package.slim-cli.debug=0 \
        --config profile.test.package.slim-lsp.debug=0 \
        --config profile.test.package.slim-tui.debug=0 \
        test --workspace)
fi

set -- build --release -p slim-cli --bin slim
if [ "$fast" = 1 ]; then
    set -- "$@" --config 'profile.release.lto="off"' --config profile.release.codegen-units=8
fi
echo "Build revision: $commit"
# option_env! acompanha esta variavel: fica restrita ao build release.
(cd "$root" && SLIM_BUILD_REVISION="$commit" cargo "$@")

mkdir -p "$install_dir"
# `install` grava um arquivo novo em vez de sobrescrever o atual: no macOS
# arm64, reescrever no lugar um binario em uso invalida a assinatura ad-hoc
# e o processo seguinte morre com "Killed: 9".
install -m 755 "$built" "$deploy.tmp"
mv -f "$deploy.tmp" "$deploy"

version=$("$deploy" --version) || {
    echo 'Smoke test do binario implantado falhou (slim --version).' >&2
    exit 1
}
case "$version" in
    "slim 0.1.0"*) ;;
    *) echo "Smoke test inesperado: $version" >&2; exit 1 ;;
esac
echo "OK: $version implantado em $deploy"

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) echo "Aviso: $install_dir nao esta no PATH; adicione ao ~/.zshrc: export PATH=\"$install_dir:\$PATH\"" ;;
esac

if [ "$keep" = 0 ]; then
    size=$(du -sh "$root/target" 2>/dev/null | cut -f1)
    (cd "$root" && cargo clean >/dev/null 2>&1) && echo "target/ removido ($size liberados; use --keep para manter)"
fi
