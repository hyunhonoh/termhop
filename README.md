# termhop — next 브랜치 (2단계 시험판)

> 이 브랜치는 개발 중인 **termhop-next** 시험판입니다.
> 지금 쓸 수 있는 1단계(PuTTY 인자를 받아 WezTerm/Windows Terminal에서 plink로 여는 런처)의 설명은
> [main 브랜치 README](https://github.com/hyunhonoh/termhop/tree/main#readme)를 보세요. 1단계 코드는 이 브랜치에도 그대로 있습니다(`src/`).

## 왜 만드나

1단계는 접속을 plink에 맡기고, 그 plink를 Windows 콘솔(ConPTY) 안에서 실행해 터미널 탭에 띄웁니다.
이 경로에서는 서버가 보낸 화면이 콘솔 변환층을 한 번 거칩니다. 그래서 `screen`처럼 화면 전체를 그리는 프로그램은
줄이 어긋날 수 있고, 문자셋(CP949)이나 창 크기 전달도 중간에서 틀어질 수 있습니다.

termhop-next는 이 중간 단계를 없앤 **창 하나짜리 SSH 터미널**입니다. 명령줄은 PuTTY와 같은 형태로 받습니다.

```
런처 ──(-l user -P 22 -pw ... host)──▶ termhop-next ─┬─ russh ────────── SSH 연결
                                                      ├─ libghostty-vt ── 터미널 화면 상태
                                                      └─ GPUI ─────────── 창·글꼴·입력(IME)
```

서버가 보낸 바이트는 변환 없이 그대로 libghostty-vt에 들어가고, 창 크기가 바뀌면 그때마다 서버에 직접 알립니다.

## 설계

| 역할 | 선택 | 이유 |
|---|---|---|
| SSH | [russh](https://github.com/warp-tech/russh) | 순수 Rust. 최신 알고리즘을 먼저 쓰고, 오래된 서버용 알고리즘(DH group1/14/GEX-SHA1, CBC, HMAC-SHA1)을 가장 낮은 우선순위로 붙임 |
| 터미널 상태 | [libghostty-vt](https://github.com/ghostty-org/ghostty) ([Rust 바인딩](https://github.com/uzaaft/libghostty-rs)) | Ghostty의 터미널 엔진. 제어 코드 해석, 스크롤백, 키 인코딩, 선택, OSC 52까지 처리 |
| 화면 | [GPUI](https://www.gpui.rs/) (Zed) | GPU로 그리는 Rust UI. 한글 등 IME 조합 입력 지원 |

코드 구성(`next/src/`):

- `args.rs` — PuTTY식 명령줄 해석. `-l`, `-pw`, `-P`, `[user@]host`. plink용 조정 옵션(`-ssh`, `-no-antispoof`, `-batch` 등)은 무시하고, 그 밖의 옵션은 오류로 알림
- `ssh.rs` — 접속·인증·셸. UI와는 채널 두 개(입력/크기 변경 ↔ 출력/알림/질문)로만 주고받음
  - 인증 순서: 비밀번호(`-pw`) → keyboard-interactive → 터미널에서 직접 입력(최대 3회)
  - 서버 키: `%APPDATA%\termhop\known_hosts`에 저장. 처음 보는 서버는 지문을 보여 주고 `y`를 받아야 접속
  - 서버에 알리는 터미널 종류: `xterm` (PuTTY와 같음)
- `term.rs` — libghostty-vt 화면을 GPUI로 그리기, 키보드·마우스·IME 처리
  - 같은 모양의 ASCII는 한 줄로 묶어 그리고, 한글·기호는 칸마다 따로 그려 대체 글꼴의 폭 차이 때문에 뒤 글자가 밀리지 않게 함
  - Enter·방향키·Ctrl 조합은 libghostty의 키 인코더로 보내고, 글자 입력은 IME를 거쳐 보냄

## 시험판 받기

[Releases의 next-alpha](https://github.com/hyunhonoh/termhop/releases/tag/next-alpha)에서 `termhop-next.exe`를 받습니다.
next 브랜치에 푸시될 때마다 새로 만들어집니다.
처음 실행할 때 "Windows의 PC 보호" 창이 뜨면 **추가 정보 → 실행**을 누르세요.

## 테스트 방법

1단계 런처를 그대로 둔 채, 같은 인자로 따로 실행해 볼 수 있습니다.

```
termhop-next.exe -l 사용자 -P 22 서버주소
termhop-next.exe -l 사용자 -pw 비밀번호 -P 22 서버주소
```

런처에서 바로 띄우려면 1단계 README의 "연결하기"처럼 PuTTY 대신 `termhop-next.exe`를 부르게 하면 됩니다.

### 확인할 것

- [ ] 접속과 로그인이 되는지(`-pw`를 줬을 때, 안 줬을 때)
- [ ] 처음 접속할 때 서버 키 질문이 나오고, `y` 후 다음 접속부터는 묻지 않는지
- [ ] `screen`에서 줄이 어긋나지 않는지(`Ctrl+A S` 분할, `Ctrl+A Tab`, 창 크기 바꾸기)
- [ ] 한글 입력: 조합 중인 글자가 커서 자리에 보이는지, 입력 후보 창이 커서 근처에 뜨는지
- [ ] 한글 출력: `ls`, `cat`으로 한글 파일 이름·내용이 칸에 맞게 보이는지
- [ ] `vi`/`top` 같은 전체 화면 프로그램, 색상
- [ ] 드래그로 선택하면 복사, 오른쪽 클릭으로 붙여넣기
- [ ] 서버에서 OSC 52로 보낸 텍스트가 PC 클립보드에 들어오는지
  (`printf "\033]52;c;$(printf hello | base64)\a"` 실행 후 메모장에 붙여넣기)

### 조작

| 동작 | 키 |
|---|---|
| 복사 | 드래그(놓으면 복사), `Ctrl+Shift+C` |
| 붙여넣기 | 오른쪽 클릭, `Ctrl+Shift+V`, `Shift+Insert` |
| 스크롤백 | 마우스 휠(전체 화면 프로그램 밖에서) |
| 글꼴 바꾸기 | 환경 변수 `TERMHOP_FONT`(기본 Consolas), `TERMHOP_FONT_SIZE`(기본 14) |

## 아직 없는 것

- 탭, 설정 파일, 저장된 PuTTY 세션(`@이름`), 키 파일 로그인(`-i`)
- 마우스 보고(전체 화면 프로그램에서 마우스 클릭·휠 전달)
- PuTTY가 저장해 둔 서버 키 기록 읽기(처음 한 번은 다시 물어봄)
- 같은 연결 안의 SFTP 패널

## 빌드

[Zig](https://ziglang.org/) 0.17이 필요합니다(libghostty-vt를 소스에서 빌드).

```
cd next
cargo build --release
```

Windows 실행 파일은 GitHub Actions(`.github/workflows/build.yml`의 `next` 작업)에서도 만들어집니다.

## 라이선스

MIT
