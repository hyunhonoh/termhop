# termhop — next 브랜치 (2단계 시험판)

> 이 브랜치는 개발 중인 **termhop-next** 시험판입니다.
> 지금 쓸 수 있는 1단계(PuTTY 인자를 받아 WezTerm/Windows Terminal에서 plink로 여는 런처)의 설명은
> [main 브랜치 README](https://github.com/hyunhonoh/termhop/tree/main#readme)를 보세요. 1단계 코드는 이 브랜치에도 그대로 있습니다(`src/`).

## 왜 만드나

1단계는 접속을 plink에 맡기고, 그 plink를 Windows 콘솔(ConPTY) 안에서 실행해 터미널 탭에 띄웁니다.
이 경로에서는 서버가 보낸 화면이 콘솔 변환층을 한 번 거칩니다. 그래서 `screen`처럼 화면 전체를 그리는 프로그램은
줄이 어긋날 수 있고, 문자셋(CP949)이나 창 크기 전달도 중간에서 틀어질 수 있습니다.

termhop-next는 이 중간 단계를 없앤 **SSH 터미널**입니다. 명령줄은 PuTTY와 같은 형태로 받습니다.
한 번 로그인한 연결 위에 탭과 분할 칸을 더 열 수 있습니다(칸마다 셸 하나, 다시 로그인하지 않음).

```
런처 ──(-l user -P 22 -pw ... host)──▶ termhop-next ─┬─ russh ────────── SSH 연결
                                                      ├─ libghostty-vt ── 터미널 화면 상태
                                                      └─ GPUI ─────────── 창·글꼴·입력(IME)
```

서버가 보낸 바이트는 변환 없이 그대로 libghostty-vt에 들어가고, 창 크기가 바뀌면 그때마다 서버에 직접 알립니다.

## 설계

| 역할 | 선택 | 이유 |
|---|---|---|
| SSH | [russh](https://github.com/warp-tech/russh) | 순수 Rust. 최신 알고리즘을 먼저 쓰고, 오래된 서버용 알고리즘(DH group1/14/GEX-SHA1, CBC, HMAC-SHA1, ssh-dss 호스트 키)을 가장 낮은 우선순위로 붙임. DH 그룹 교환 크기는 PuTTY와 같은 1024~8192비트로 요청(`next/vendor/russh`, 한 줄 수정) |
| 터미널 상태 | [libghostty-vt](https://github.com/ghostty-org/ghostty) ([Rust 바인딩](https://github.com/uzaaft/libghostty-rs)) | Ghostty의 터미널 엔진. 제어 코드 해석, 스크롤백, 키 인코딩, 선택, OSC 52까지 처리 |
| 화면 | [GPUI](https://www.gpui.rs/) (Zed) | GPU로 그리는 Rust UI. 한글 등 IME 조합 입력 지원 |

코드 구성(`next/src/`):

- `args.rs` — PuTTY식 명령줄 해석. `-l`, `-pw`, `-P`, `[user@]host`. plink용 조정 옵션(`-ssh`, `-no-antispoof`, `-batch` 등)은 무시하고, 그 밖의 옵션은 오류로 알림
- `ssh.rs` — 접속·인증·셸. 연결은 하나, 칸마다 그 연결 위에 세션 채널을 하나씩 엶. 칸과는 채널 두 개(입력/크기 변경 ↔ 출력/알림/질문)로만 주고받음
  - 인증 순서: 비밀번호(`-pw`) → keyboard-interactive → 터미널에서 직접 입력(최대 3회)
  - 서버 키: `%APPDATA%\termhop\known_hosts`에 저장. 처음 보는 서버는 지문을 보여 주고 `y`를 받아야 접속
  - 서버에 알리는 터미널 종류: `xterm` (PuTTY와 같음)
- `config.rs` — `next.conf`(글꼴, 크기, 터미널 종류, 스크롤백, 색)
- `workspace.rs` — 탭과 분할 칸 배치, 탭·분할 단축키, 칸 사이 이동
- `term.rs` — 칸 하나: libghostty-vt 화면을 GPUI로 그리기, 키보드·마우스·IME 처리
  - 같은 모양의 ASCII는 한 줄로 묶어 그리고, 한글·기호는 칸마다 따로 그려 대체 글꼴의 폭 차이 때문에 뒤 글자가 밀리지 않게 함
  - Enter·방향키·Ctrl 조합은 libghostty의 키 인코더로 보내고, 글자 입력은 IME를 거쳐 보냄
  - 프로그램이 마우스를 요청하면 libghostty의 마우스 인코더로 클릭·드래그·휠을 보냄(SGR 등 프로그램이 고른 형식)

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
- [ ] 마우스를 쓰는 프로그램(`tmux`의 `set -g mouse on`, `vim`의 `:set mouse=a`)에서 클릭·드래그·휠이 그 프로그램으로 가는지,
  Shift를 누른 채 드래그하면 termhop 쪽 선택·복사가 되는지
- [ ] `next.conf`로 글꼴·크기·색을 바꿔 보기(아래 "설정")
- [ ] 새 탭·분할(`Ctrl+Shift+T`, `Alt+Shift+=`)에 셸이 뜨는지. 접속하는 서버가 연결 하나에 셸 하나만 허용하면
  새 칸에 그 내용이 표시됨(안 되는 서버인지 확인하는 항목)

### 조작

| 동작 | 키 |
|---|---|
| 복사 | 드래그(놓으면 복사), `Ctrl+Shift+C` |
| 붙여넣기 | 오른쪽 클릭, `Ctrl+Shift+V`, `Shift+Insert` |
| 스크롤백 | 마우스 휠(전체 화면 프로그램 밖에서) |
| 마우스를 쓰는 프로그램 안에서 선택·붙여넣기 | `Shift`를 누른 채 드래그 / 오른쪽 클릭 |
| 새 탭 | `Ctrl+Shift+T` (Mac `Cmd+T`), 탭 줄의 `+` |
| 오른쪽으로 분할 | `Alt+Shift+=` (Mac `Cmd+D`) |
| 아래로 분할 | `Alt+Shift+-` (Mac `Cmd+Shift+D`) |
| 칸 닫기 | `Ctrl+Shift+W` (Mac `Cmd+W`), 탭은 탭의 `×`. 셸이 끝난 칸은 아무 키나 |
| 탭 이동 | `Ctrl+Tab` / `Ctrl+Shift+Tab`, `Ctrl+Alt+1~9` (Mac `Cmd+1~9`) |
| 칸 이동 | `Alt+방향키` (Mac `Cmd+Option+방향키`), 또는 클릭. 칸이 하나면 `Alt+방향키`는 서버로 감 |
| 창 닫기 | 창의 X, 또는 마지막 칸 닫기 |

단축키는 Windows Terminal(Mac은 Ghostty)과 같습니다. 탭 줄은 탭이 두 개 이상일 때만 보입니다.
새 탭과 칸은 처음 접속한 연결을 함께 쓰므로, 그 연결이 끊기면 모든 칸이 같이 끝납니다.

프로그램이 마우스를 요청하면(tmux 마우스 모드, `vim`의 `mouse=a` 등) 클릭·드래그·휠은 그 프로그램으로 갑니다.
그 프로그램이 복사한 내용은 OSC 52로 PC 클립보드에 들어옵니다(tmux는 `set -g set-clipboard on`).

## 설정 (선택)

`termhop-next.exe`와 같은 폴더, 또는 `%APPDATA%\termhop\`에 `next.conf`를 **UTF-8**로 저장합니다.
1단계의 `termhop.conf`와는 다른 파일입니다. 모든 항목은 생략할 수 있습니다.

**Ghostty 설정을 그대로 쓸 수 있습니다.** Ghostty의 `config` 파일 내용을 `next.conf`에 붙여 넣으면
글꼴·크기·색·테마를 읽고, 창·단축키 같은 나머지 항목은 건너뜁니다.

```ini
# 테마 (Ghostty 테마 이름. light:이름,dark:이름 이면 dark를 씀)
theme = Catppuccin Frappe

# 글꼴과 크기 (font-family / font-size 도 됨)
font-family = JetBrainsMonoHangulNL Nerd Font Mono
font-size = 16

# 서버에 알리는 터미널 종류 (기본 xterm, PuTTY와 같음)
term = xterm-256color

# 스크롤백 줄 수 (기본 10000)
scrollback = 10000

# 색을 직접 정하면 테마보다 우선
# background = #303446
# foreground = #c6d0f5
# cursor-color = #f2d5cf
# cursor-text = #303446
# selection-background = #f2d5cf
# selection-foreground = #303446
# palette = 1=#e78284
```

- **테마**: `Catppuccin Mocha / Macchiato / Frappe / Latte`는 내장돼 있습니다. 다른 Ghostty 테마는 Mac의
  `/Applications/Ghostty.app/Contents/Resources/ghostty/themes/`에서 파일을 복사해 `next.conf` 옆 `themes` 폴더에 넣으면 됩니다.
- **글꼴**: Windows에 설치된 글꼴만 쓸 수 있습니다. 없으면 창 첫 줄에 알리고 Consolas로 엽니다.
- 잘못된 줄은 창 첫 줄에 몇 번째 줄인지 표시하고 건너뜁니다.
- 서버 키 기록(`known_hosts`)도 같은 `%APPDATA%\termhop\` 폴더에 저장됩니다.

## 아직 없는 것

- 칸 크기 조절(지금은 반씩 나눔), 저장된 PuTTY 세션(`@이름`), 키 파일 로그인(`-i`)
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
