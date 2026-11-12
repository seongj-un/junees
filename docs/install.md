# 설치, 업데이트, 릴리스

## 설치

### 설치 스크립트

```sh
curl -fsSL https://raw.githubusercontent.com/seongj-un/junees/main/install.sh | sh
```

GitHub 릴리스에서 이 컴퓨터에 맞는 빌드를 받아 `~/.local/bin/junees`에 둔다. sudo는 쓰지 않는다.

1. `uname`으로 OS와 CPU를 본다. 릴리스 빌드는 Linux(x86_64, aarch64, musl 정적 링크라 배포판을 가리지 않는다)와 macOS(x86_64, Apple silicon)용이 있다. Rosetta 셸에서 돌면 Apple silicon 빌드를 고른다. 그 밖의 OS와 CPU는 거절하고 소스 빌드를 안내한다.
2. 최신 릴리스는 GitHub API를 부르지 않고 `releases/latest/download/SHA256SUMS`(최신 릴리스로 가는 리다이렉트)를 받아, 그 안의 파일 이름 `junees-<버전>-<타깃>.tar.gz`에서 버전을 읽는다.
3. 그 버전의 `junees-<버전>-<타깃>.tar.gz`를 받아 SHA-256을 `SHA256SUMS`와 맞춰 본다. 다르면 아무것도 설치하지 않고 멈춘다.
4. 압축에서 `junees` 하나만 꺼내 설치 폴더에 임시 이름으로 복사하고 `mv`로 바꿔 넣는다(있던 junees는 새 파일이 다 쓰인 뒤에 한 번에 바뀐다).
5. 설치 폴더가 PATH에 없으면 더할 줄을 알려 준다.

임시 폴더(`$TMPDIR` 아래 `junees-install.*`)와 설치 폴더의 임시 복사본(`.junees.install.<pid>`)은 끝나거나 실패하거나 중단되면 지운다. 필요한 것: `curl` 또는 `wget`(BusyBox wget도 된다: wget에는 두 구현에 다 있는 `-q -O`만 넘긴다), `tar`, `sha256sum`/`shasum`/`openssl` 중 하나. 받기가 실패하면 어느 주소를 무엇(`curl`/`wget`)으로 받다 실패했는지 `download failed (...)`로 알려 준다.

| 변수 | 뜻 |
|---|---|
| `JUNEES_INSTALL_DIR` | 설치 폴더(기본 `~/.local/bin`) |
| `JUNEES_VERSION` | 설치할 릴리스(`v0.1.0` 또는 `0.1.0`, 기본 최신) |
| `JUNEES_DOWNLOAD_BASE` | 테스트용. 릴리스 주소를 이 컴퓨터의 서버(`http://127.0.0.1:...`)로 바꾼다. 다른 주소와 `@`(사용자 정보: `http://127.0.0.1:1@다른곳`)가 든 주소는 거절 |

```sh
curl -fsSL https://raw.githubusercontent.com/seongj-un/junees/main/install.sh | JUNEES_VERSION=v0.1.0 JUNEES_INSTALL_DIR=~/bin sh
```

### 직접 받기

[릴리스 페이지](https://github.com/seongj-un/junees/releases)에서 `junees-<버전>-<타깃>.tar.gz`와 `SHA256SUMS`를 받는다.

```sh
sha256sum -c --ignore-missing SHA256SUMS        # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
tar -xzf junees-0.1.0-x86_64-unknown-linux-musl.tar.gz
install -m 755 junees-0.1.0-x86_64-unknown-linux-musl/junees ~/.local/bin/junees
```

릴리스 파일에는 GitHub Actions가 만든 출처 증명(SLSA build provenance)이 붙는다. 이 저장소의 릴리스 워크플로가 만든 파일인지 확인하려면:

```sh
gh attestation verify junees-0.1.0-x86_64-unknown-linux-musl.tar.gz --repo seongj-un/junees
```

### 소스에서

Rust 1.88 이상(edition 2024)이 필요하다.

```sh
cargo install --git https://github.com/seongj-un/junees
# 또는 clone 뒤
cargo install --path .
```

## 업데이트

```sh
junees update --check          # 새 릴리스가 있는지만
junees update                  # 최신 릴리스가 더 새로우면 바꿔 넣기
junees update --version v0.1.0 # 그 릴리스로(더 오래된 것도)
```

대화에서는 `/update`가 `--check`와 같다(설치는 하지 않는다). `junees doctor --online`도 같은 확인을 `online`/`release` 줄로 보여 준다(새 버전이 있으면 경고).

`junees update`가 하는 일:

1. `https://github.com/seongj-un/junees/releases/latest`의 리다이렉트(따라가지 않고 `Location`만 읽는다)로 최신 태그를 알아낸다. 로그인도 GitHub API 요청 한도도 쓰지 않는다. 릴리스가 없으면 그렇다고 말한다.
2. 지금 버전(빌드한 `Cargo.toml` 버전)과 semver로 비교한다. 같거나 지금 것이 더 새로우면 아무것도 하지 않는다.
3. 이 컴퓨터의 타깃(위 네 가지) 압축과 `SHA256SUMS`를 받는다. https만, 리다이렉트는 `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`, `github-releases.githubusercontent.com`로만 따라간다. 크기 상한은 압축 100 MiB, `SHA256SUMS` 64 KiB.
4. 압축의 SHA-256이 `SHA256SUMS`와 다르면 멈춘다(아무것도 바꾸지 않는다).
5. 시스템 `tar`로 비공개 임시 폴더에 `junees-<버전>-<타깃>/junees` 하나만 꺼내고(일반 파일이어야 한다), 지금 실행 중인 프로그램(`current_exe`, 심볼릭 링크면 가리키는 실제 파일)과 같은 폴더에 임시 파일로 써서 원래 권한을 주고 fsync한 뒤 `rename`으로 바꿔 넣는다. 도중에 실패하면 임시 파일을 지우고, 원래 파일은 바뀌기 전까지 온전하다.

안내:

- 그 폴더에 쓸 권한이 없으면(sudo로 `/usr/local/bin`에 넣은 경우 등) 바꾸지 않고, 설치한 사용자로 실행하거나 `install.sh`로 내 폴더(`JUNEES_INSTALL_DIR`)에 다시 설치하라고 알린다.
- `cargo install`로 넣은 바이너리(`$CARGO_HOME/bin`, 기본 `~/.cargo/bin`)도 그 자리에서 바꾸고, 나중에 `cargo install`이 자기 빌드로 다시 덮는다고 알린다.
- 디스코드/텔레그램 게이트웨이, cron 데몬, 웹훅 서버가 돌고 있으면(각자의 잠금 파일로 판단) 옛 바이너리로 계속 돌므로 다시 시작하라고 알린다.

`junees update`(`--check` 포함)는 Junees 자신의 상태를 바꾸는 명령 목록(`self_command::STATE_CHANGING`)에 있다. 모델의 bash가 실행하려 하면 대화와 게이트웨이에서는 묻고, 묻는 사람이 없는 실행(`-p`, 작업 모드, 예약 작업, 웹훅)에서는 거절하며, claude-code 백엔드에는 거부 규칙으로 들어간다.

`JUNEES_DOWNLOAD_BASE`는 `junees update`에도 같은 뜻(테스트용, 루프백 주소만)이다.

## macOS

릴리스 빌드와 CI(`cargo check --all-targets`, 경고는 오류)가 macOS를 덮는다. Linux 전용 기능은 macOS에서 꺼진다.

- bash 실행 격리(Landlock): 없다. `sandbox = auto`(대화, `-p`, 작업 모드의 기본)는 첫 결과에 격리 없이 실행한다고 알리고, `require`(예약 작업, 웹훅의 기본)는 명령을 거절한다. macOS에서 예약 작업이나 웹훅 실행에 bash를 쓰려면 `sandbox = off`로 명시해야 한다. `junees doctor`의 sandbox 줄이 이 상태를 보여 준다.
- 환경 변수 보호(`PR_SET_DUMPABLE`): 없다. 같은 사용자의 다른 프로세스가 junees의 환경(API 키 등)을 읽을 수 있다.
- claude-code 백엔드를 멈출 때 `/proc`로 자손 프로세스를 찾아 끝내는 부분이 동작하지 않아, 프로세스 그룹 신호만 간다. Claude Code가 따로 띄운(세션을 분리한) 명령은 남을 수 있다. bubblewrap 확인은 Linux에서만 한다.
- 브라우저 도구는 닫을 때 브라우저가 스스로 끝나기를 기다리지 않고 바로 프로세스 그룹을 끝낸다.

## 릴리스 만들기

릴리스는 `.github/workflows/release.yml`이 만든다.

1. `Cargo.toml`의 `version`을 올리고(`Cargo.lock`도 함께) main에 merge한다.
2. 그 커밋에 태그를 달아 push한다. 태그는 `v` + `Cargo.toml` 버전이어야 한다.

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. 워크플로:
   - `version`: `cargo metadata --locked`로 읽은 버전과 태그를 비교해 다르면 실패.
   - `build`(4개): `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`은 ubuntu 러너에서 `cargo-zigbuild`(zig가 ring의 C 코드 컴파일과 musl 링크를 맡는다, pip 해시 고정), `x86_64-apple-darwin`, `aarch64-apple-darwin`은 macOS 러너(Apple silicon)에서 `cargo build`. 각각 `junees-<버전>-<타깃>.tar.gz`(바이너리, LICENSE, README.md)를 만들고, 러너에서 돌릴 수 있는 둘은 `--version`이 버전과 맞는지 본다.
   - `checksums`: 네 압축의 `SHA256SUMS`.
   - `attest`: `actions/attest`로 압축과 `SHA256SUMS`의 출처 증명(`id-token: write`, `attestations: write`는 이 job만).
   - `release`: `gh release create --verify-tag --generate-notes`(직전 태그 이후 merge된 PR 목록). 버전에 `-`가 있으면(`0.2.0-rc.1`) pre-release. `contents: write`는 이 job만.
4. 끝나면 릴리스 페이지의 파일과 `install.sh`로 설치를 확인한다.

시험 실행(릴리스는 만들지 않음): Actions의 Release 워크플로를 `publish`를 끈 채 실행하면(`gh workflow run release.yml --ref <브랜치>`) 빌드와 `SHA256SUMS`까지 하고 결과를 워크플로 아티팩트로 남긴다. `workflow_dispatch`는 워크플로 파일이 기본 브랜치에 있어야 쓸 수 있으므로, 처음 들어가는 PR이나 `release.yml`, `Cargo.toml`, `Cargo.lock`을 바꾸는 PR에서는 `pull_request` 트리거가 같은 시험 실행을 한다(의존성이 바뀌어 musl 정적 빌드가 깨지면 태그 전에 드러난다). `publish`를 켠 수동 실행은 `v*` 태그에서만 된다.

워크플로의 모든 액션은 커밋 SHA로 고정했고(주석에 버전), 기본 권한은 없음(`permissions: {}`)에서 job마다 필요한 것만 준다. 릴리스 빌드는 캐시를 쓰지 않는다.
