# ConPTY offline matrix

- executable: `\\?\C:\Users\User\bin\Slim.exe`
- bytes: 13830144
- SHA-256: `DBFE58EE27FCE7D2F2001D88A7A4F0A6591F58A9AE911D0070A87B2FBD0008C3`
- source: `SLIM_E2E_EXE`
- provider: in-process loopback only
- credential: sentinel, never persisted

| case | size | reduced | no color | raw bytes | normalized bytes | outcome |
|---|---:|:---:|:---:|---:|---:|---|
| 120x30-normal | 120x30 | false | false | - | - | FAIL: startup emitted no rendered SLIM frame; early_exit=None; source=SLIM_E2E_EXE path=\\?\C:\Users\User\bin\Slim.exe bytes=13830144 sha256=DBFE58EE27FCE7D2F2001D88A7A4F0A6591F58A9AE911D0070A87B2FBD0008C3 |
