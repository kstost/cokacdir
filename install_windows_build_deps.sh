#!/bin/bash
# Windows 크로스 컴파일에 필요한 시스템 패키지 설치

set -e

if [ "$(id -u)" -ne 0 ]; then
    echo "root 권한이 필요합니다. sudo로 실행해주세요."
    exit 1
fi

# 먼저 기본 LLVM 도구를 설치한 뒤 실제 `clang` 기본 명령의 주 버전을
# 사용한다. 여러 버전이 공존할 때 glob의 첫(가장 오래된) 항목을 고르지 않는다.
apt update
apt install -y clang lld llvm

LLVM_VERSION="$(clang -dumpversion 2>/dev/null | cut -d. -f1)"
if ! [[ "$LLVM_VERSION" =~ ^[0-9]+$ ]]; then
    LLVM_VERSION="$(find /usr/bin -maxdepth 1 -type f -name 'clang-[0-9]*' -printf '%f\n' \
        | sed -n 's/^clang-\([0-9][0-9]*\)$/\1/p' | sort -V | tail -1)"
fi
if [ -z "$LLVM_VERSION" ]; then
    echo "LLVM 버전을 탐지할 수 없습니다."
    exit 1
fi

echo "탐지된 LLVM 버전: ${LLVM_VERSION}"

apt install -y "clang-tools-${LLVM_VERSION}"

# 버전 없는 심볼릭 링크 생성 (cargo-xwin이 필요로 함)
for tool in llvm-lib llvm-dlltool llvm-rc clang-cl; do
    if [ -f "/usr/bin/${tool}-${LLVM_VERSION}" ] && [ ! -e "/usr/bin/${tool}" ]; then
        ln -s "${tool}-${LLVM_VERSION}" "/usr/bin/${tool}"
        echo "심볼릭 링크 생성: ${tool} → ${tool}-${LLVM_VERSION}"
    elif [ -f "/usr/bin/${tool}" ]; then
        echo "이미 존재: /usr/bin/${tool}"
    else
        echo "경고: /usr/bin/${tool}-${LLVM_VERSION} 을 찾을 수 없습니다"
    fi
done

missing=0
for tool in clang ld.lld llvm-lib llvm-dlltool llvm-rc clang-cl; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "오류: 필수 도구를 찾을 수 없습니다: $tool" >&2
        missing=1
    fi
done
if [ "$missing" -ne 0 ]; then
    exit 1
fi

echo ""
echo "설치 완료:"
clang --version | head -1
ld.lld --version | head -1
llvm-lib --version 2>/dev/null | head -1 || echo "llvm-lib: 미설치"
clang-cl --version 2>/dev/null | head -1 || echo "clang-cl: 미설치"
