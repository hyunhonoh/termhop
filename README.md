# termhop

PuTTY를 실행하도록 되어 있는 런처(웹 접속 버튼, 접속 관리 도구 등)에서 넘어오는 인자를 그대로 받아,
**WezTerm** 또는 **Windows Terminal**의 새 탭에서 세션을 엽니다. 실제 접속은 PuTTY의 명령줄 버전인
**plink**가 하므로, PuTTY로 되던 접속은 그대로 됩니다.

```
런처 ──(-l user -P 22 -pw ... host)──▶ termhop ──▶ WezTerm 새 탭 ─▶ plink -ssh -l user -P 22 ... host
```

- 이미 열린 WezTerm 창이 있으면 탭으로 추가하고, 없으면 새 창을 엽니다.
- 인자는 손대지 않고 plink에 넘깁니다(`-l`, `-P`, `-pw`, `-i`, `-load`, `-L` 등). 프로토콜 옵션이 없으면 `-ssh`만 붙이고,
  PuTTY의 `@세션이름` 표기는 `-load 세션이름`으로 바꿉니다.
- 의존성 없는 작은 실행 파일 하나입니다.

## 설치

1. [Releases](https://github.com/hyunhonoh/termhop/releases)에서 `termhop-v<버전>-windows-x64.exe`를 받아
   원하는 폴더에 두고 `termhop.exe`로 이름을 바꿉니다.
   - 처음 실행할 때 "Windows의 PC 보호" 창이 뜨면 **추가 정보 → 실행**을 누르거나, 파일 속성에서 **차단 해제**를 체크합니다.
2. plink와 터미널을 설치합니다.
   ```
   winget install PuTTY.PuTTY
   winget install wez.wezterm
   ```
   Windows Terminal은 Windows 11에 기본으로 들어 있습니다.

## 연결하기

런처가 PuTTY를 부르는 방식에 맞춰 termhop을 대신 부르게 하면 됩니다.

- **런처 설정에 실행 파일 경로 항목이 있으면**: `termhop.exe` 경로로 바꿉니다.
- **런처가 정해진 경로의 실행 파일을 부르면**: 그 파일을 다른 이름으로 바꿔 두고(예: `putty.exe` → `putty-original.exe`),
  `termhop.exe`를 그 자리에 원래 이름으로 복사합니다.
  이 경우 설정 파일은 `%APPDATA%\termhop\termhop.conf`에 두는 것을 권장합니다(Program Files는 쓰기 권한이 필요).
- **URL 프로토콜로 실행되면**: 해당 레지스트리 명령의 실행 파일을 `termhop.exe`로 바꿉니다.

**되돌리기**: 위에서 바꾼 경로, 파일 이름, 레지스트리 값을 원래대로 돌려놓으면 PuTTY로 돌아갑니다.

## 처음 접속할 때

- 처음 접속하는 서버는 탭 안에서 plink가 `Store key in cache? (y/n, ...)`를 묻습니다. `y`를 누르면 됩니다.
  PuTTY로 이미 접속했던 서버는 같은 기록을 쓰므로 묻지 않습니다.
- **WezTerm은 기본 설정에서 접속이 끝나면 탭을 바로 닫습니다.** 접속에 실패했을 때 메시지를 보려면
  `wezterm.lua`에 다음 줄을 추가하세요.
  ```lua
  config.exit_behavior = "CloseOnCleanExit"
  ```
- **탭이 열렸다가 바로 닫히면**: 위 설정을 넣고 다시 접속해 메시지를 확인하세요.
  `unknown option "-no-antispoof"`가 보이면 plink가 0.71보다 오래된 것입니다.
  `winget upgrade PuTTY.PuTTY`로 업데이트하거나, 설정에 `access_prompt = on`을 적으세요.

## 설정 (선택)

`termhop.exe`와 같은 폴더, 또는 `%APPDATA%\termhop\`에 `termhop.conf`를 **UTF-8**로 저장합니다.
설정 파일이 없으면 PATH와 기본 설치 경로에서 프로그램을 찾고 WezTerm을 씁니다.

```ini
# wezterm(기본) 또는 wt
terminal = wezterm

# 로그인 뒤 "Press Return to begin session"에서 Enter를 눌러야 세션이 시작되게 하려면 on
# (기본 off: PuTTY처럼 바로 들어갑니다)
# access_prompt = on

# 자동으로 못 찾을 때만 경로를 적습니다
# plink = C:\Program Files\PuTTY\plink.exe
# wezterm = C:\Program Files\WezTerm\wezterm.exe
# wt = C:\Users\me\AppData\Local\Microsoft\WindowsApps\wt.exe
```

## 확인

`termhop.exe`를 인자 없이 실행하면 사용법과 설정 파일 위치가 나옵니다.

`--termhop-dry-run`을 붙이면 실행하지 않고 실행할 명령만 보여 줍니다(`-pw` 값은 `****`로 표시).

```
termhop --termhop-dry-run -l user -P 22 example.com
```

그다음 `Win+R`에서 `termhop.exe -l 사용자 서버주소`를 실행해 탭이 열리는지 보고, 런처에 연결합니다.

## 빌드

```
cargo build --release
```

Windows 실행 파일은 GitHub Actions(`.github/workflows/build.yml`)에서도 만들어집니다.
`v*` 태그를 푸시하면 Releases에 `termhop-v<버전>-windows-x64.exe`가 첨부됩니다.

## 라이선스

MIT
