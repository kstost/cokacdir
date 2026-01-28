# COKACDIR Rust Build System 사용 설명서

Linux 및 macOS용 크로스 컴파일을 지원하는 Python 기반 빌드 시스템입니다.
모든 빌드 도구는 `builder/tools/` 폴더에 로컬 설치되어 시스템 환경을 오염시키지 않습니다.

## 요구 사항

- Python 3.6 이상
- 인터넷 연결 (도구 다운로드용)
- 약 500MB 디스크 공간 (모든 도구 설치 시)

## 빠른 시작

```bash
# 현재 플랫폼용 빌드 (도구 자동 설치)
python3 build.py

# macOS용 크로스 컴파일
python3 build.py --macos

# 모든 플랫폼용 빌드
python3 build.py --all
```

## 명령어 옵션

### 빌드 모드

| 옵션 | 설명 |
|------|------|
| `--debug` | 디버그 모드로 빌드 (빠른 컴파일, 최적화 없음) |
| `--release` | 릴리스 모드로 빌드 (기본값, 최적화 적용) |
| `--clean` | 빌드 전 기존 아티팩트 삭제 |

### 타겟 선택

| 옵션 | 설명 |
|------|------|
| `--native` | 현재 플랫폼용 빌드 (기본값) |
| `--macos` | macOS 양쪽 아키텍처 (arm64 + x86_64) |
| `--macos-arm64` | macOS Apple Silicon (M1/M2/M3/M4) |
| `--macos-x86_64` | macOS Intel |
| `--linux` | Linux 양쪽 아키텍처 (arm64 + x86_64) |
| `--linux-arm64` | Linux ARM64 |
| `--linux-x86_64` | Linux x86_64 |
| `--all` | 모든 지원 플랫폼 |

### 설정 옵션

| 옵션 | 설명 |
|------|------|
| `--setup` | 모든 빌드 도구 설치 (Rust, Zig, cargo-zigbuild, macOS SDK) |
| `--setup-rust` | Rust 툴체인만 설치 |
| `--setup-cross` | 크로스 컴파일 도구만 설치 (Zig, cargo-zigbuild, macOS SDK) |
| `--status` | 설치된 도구 상태 확인 |

### 기타 옵션

| 옵션 | 설명 |
|------|------|
| `--verbose`, `-v` | 상세 출력 활성화 |
| `--no-color` | 색상 출력 비활성화 |
| `--no-auto-setup` | 누락된 도구 자동 설치 비활성화 |
| `--help`, `-h` | 도움말 표시 |

## 사용 예제

### 1. 도구 상태 확인

```bash
python3 build.py --status
```

출력 예시:
```
==================================================
  Tool Status
==================================================

✓ Rust: /path/to/builder/tools/cargo/bin/cargo
✓ Zig: /path/to/builder/tools/zig-0.13.0/zig
✓ cargo-zigbuild: Installed
✓ macOS SDK: /path/to/builder/tools/MacOSX14.0.sdk
```

### 2. 모든 도구 설치

```bash
python3 build.py --setup
```

### 3. 현재 플랫폼용 빌드

```bash
# 릴리스 빌드 (기본값)
python3 build.py

# 디버그 빌드
python3 build.py --debug
```

### 4. macOS용 크로스 컴파일

```bash
# Apple Silicon + Intel Mac 모두
python3 build.py --macos

# Apple Silicon만
python3 build.py --macos-arm64

# Intel Mac만
python3 build.py --macos-x86_64
```

### 5. 모든 플랫폼 빌드

```bash
python3 build.py --all
```

### 6. 클린 빌드

```bash
# 클린 후 모든 플랫폼 빌드
python3 build.py --clean --all
```

### 7. 릴리스 빌드 (최적화)

```bash
python3 build.py --release --all
```

## 빌드 결과물

빌드된 바이너리는 `dist/` 폴더에 저장됩니다:

| 파일명 | 대상 플랫폼 |
|--------|-------------|
| `cokacdir-linux-aarch64` | Linux ARM64 |
| `cokacdir-linux-x86_64` | Linux x86_64 |
| `cokacdir-macos-aarch64` | macOS Apple Silicon (M1/M2/M3/M4) |
| `cokacdir-macos-x86_64` | macOS Intel |

## 설치되는 도구

`builder/tools/` 폴더에 다음 도구들이 설치됩니다:

| 도구 | 용도 | 크기 |
|------|------|------|
| Rust (cargo, rustup) | Rust 컴파일러 및 패키지 매니저 | ~300MB |
| Zig | 크로스 컴파일용 C/C++ 툴체인 | ~40MB |
| cargo-zigbuild | Zig를 사용한 Rust 크로스 컴파일 | ~4MB |
| macOS SDK | macOS 크로스 컴파일용 SDK | ~70MB |

## 폴더 구조

```
project/
├── build.py              # 메인 빌드 스크립트
├── builder/
│   ├── __init__.py       # 패키지 초기화
│   ├── config.py         # 빌드 설정
│   ├── executor.py       # 빌드 실행
│   ├── logger.py         # 로깅
│   ├── targets.py        # 타겟 관리
│   ├── tools.py          # 도구 설치 관리
│   └── tools/            # 설치된 도구들
│       ├── cargo/        # Rust cargo
│       ├── rustup/       # Rust rustup
│       ├── zig-0.13.0/   # Zig 컴파일러
│       └── MacOSX14.0.sdk/  # macOS SDK
└── dist/                 # 빌드 결과물
    ├── cokacdir-linux-aarch64
    ├── cokacdir-linux-x86_64
    ├── cokacdir-macos-aarch64
    └── cokacdir-macos-x86_64
```

## 문제 해결

### Rust 설치 실패

```bash
# Rust만 다시 설치
rm -rf builder/tools/cargo builder/tools/rustup
python3 build.py --setup-rust
```

### 크로스 컴파일 도구 재설치

```bash
# 크로스 컴파일 도구만 다시 설치
rm -rf builder/tools/zig-* builder/tools/MacOSX*.sdk*
python3 build.py --setup-cross
```

### 전체 도구 재설치

```bash
# 모든 도구 삭제 후 재설치
rm -rf builder/tools
python3 build.py --setup
```

### 빌드 캐시 삭제

```bash
# 빌드 캐시 및 결과물 삭제
python3 build.py --clean
```

## 환경 변수

빌드 시스템은 다음 환경 변수를 자동으로 설정합니다:

| 변수 | 값 |
|------|-----|
| `CARGO_HOME` | `builder/tools/cargo` |
| `RUSTUP_HOME` | `builder/tools/rustup` |
| `SDKROOT` | `builder/tools/MacOSX14.0.sdk` (macOS 크로스 컴파일 시) |
| `PATH` | cargo/bin 및 zig 경로 추가 |

## 지원 플랫폼

### 빌드 호스트
- Linux x86_64
- Linux ARM64 (aarch64)
- macOS x86_64
- macOS ARM64 (Apple Silicon)

### 빌드 타겟
- Linux x86_64 (`x86_64-unknown-linux-gnu`)
- Linux ARM64 (`aarch64-unknown-linux-gnu`)
- macOS x86_64 (`x86_64-apple-darwin`)
- macOS ARM64 (`aarch64-apple-darwin`)
