#!/usr/bin/env bash
set -euo pipefail
rustc=$1; shift
printf '%s\n' "$*" >> /review/logs/outer-rustc-invocations.log
if [[ $# == 1 && $1 == -vV ]]; then exec "$rustc" "$@"; fi
if [[ ${1:-} == - && ${2:-} == --crate-name && ${3:-} == ___ ]]; then
    args=("$@"); shift 3
    good=1; printed=0
    while (($#)); do
        case $1 in
            --print=file-names|--print=sysroot|--print=split-debuginfo|--print=crate-name|--print=cfg) printed=1; shift ;;
            --crate-type) case ${2:-} in bin|rlib|dylib|cdylib|staticlib|proc-macro) shift 2 ;; *) good=0; break ;; esac ;;
            -Wwarnings) shift ;;
            *) good=0; break ;;
        esac
    done
    if ((good && printed)); then exec "$rustc" "${args[@]}"; fi
fi
printf '%s\n' "REFUSED $*" >> /review/logs/outer-rustc-refused.log
echo 'independent outer guard refused compilation' >&2
exit 101
