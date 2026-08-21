; Waypad host installer.
;
; Two decisions shape this script.
;
; It installs per user and asks for no administrator rights. Waypad injects
; input and duplicates the desktop, both of which only work inside the user's
; own interactive session, so a machine-wide install would buy nothing and cost
; a UAC prompt on every update. The one thing that does need elevation — a
; firewall rule — is an optional task that raises its own prompt when chosen.
;
; Nothing is started or registered behind the user's back. Login start is a
; checkbox that defaults to on because a remote-control daemon that is not
; running when you reach for your phone is useless, but it is theirs to clear.

#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif

#define AppName "Waypad"
#define AppPublisher "Waypad"
#define AppExeName "waypad-daemon.exe"
#define AppUrl "https://github.com/Frumorn12/waypad-deamon"

[Setup]
AppId={{9E2F6A1C-3C77-4B58-9E0B-6C1E2C7A4F31}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppSupportURL={#AppUrl}
AppUpdatesURL={#AppUrl}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
; Per user, no elevation. See the note at the top of the file.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputBaseFilename=WaypadSetup-{#AppVersion}
SetupIconFile=..\..\crates\waypad-daemon\assets\waypad.ico
UninstallDisplayIcon={app}\{#AppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Waypad holds the desktop duplication handle and an open control port, so a
; running copy has to go before a new one lands.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"

[Tasks]
Name: "autostart"; Description: "Start {#AppName} when I sign in"; GroupDescription: "Startup"
; Off by default: it raises a UAC prompt, and Windows asks about the firewall
; on its own the first time the daemon listens. This is for people who would
; rather answer it once, here, than in a pop-up later.
Name: "firewall"; Description: "Add a Windows Firewall rule for your local network (asks for administrator)"; GroupDescription: "Network"; Flags: unchecked

[Files]
Source: "..\..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\crates\waypad-daemon\assets\waypad.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion isreadme

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Parameters: "serve"; IconFilename: "{app}\waypad.ico"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Registry]
; Per-user login start, which is what the control panel's toggle also writes.
; uninsdeletevalue rather than a whole key: this key belongs to Windows and is
; full of other applications' entries.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
  ValueType: string; ValueName: "Waypad"; \
  ValueData: """{app}\{#AppExeName}"" serve"; \
  Flags: uninsdeletevalue; Tasks: autostart

[Run]
; shellexec with runas is what raises UAC for this one command; the installer
; itself stays unelevated.
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall add rule name=""Waypad control"" dir=in action=allow protocol=TCP localport=47771 profile=private"; \
  Flags: shellexec runhidden waituntilterminated; Verb: "runas"; Tasks: firewall; \
  StatusMsg: "Adding the firewall rule..."
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall add rule name=""Waypad discovery"" dir=in action=allow protocol=UDP localport=47770 profile=private"; \
  Flags: shellexec runhidden waituntilterminated; Verb: "runas"; Tasks: firewall
; Launched without waiting: the daemon runs until it is told to stop, and the
; installer must not sit on its exit code.
Filename: "{app}\{#AppExeName}"; Parameters: "serve"; \
  Description: "Start {#AppName} now"; Flags: postinstall nowait skipifsilent

[UninstallRun]
; Best effort. A user who declined the firewall task has no rule to remove, and
; netsh says so with a non-zero exit that must not fail the uninstall.
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall delete rule name=""Waypad control"""; \
  Flags: shellexec runhidden waituntilterminated skipifdoesntexist; Verb: "runas"; \
  RunOnceId: "RemoveWaypadControlRule"
Filename: "{sys}\netsh.exe"; \
  Parameters: "advfirewall firewall delete rule name=""Waypad discovery"""; \
  Flags: shellexec runhidden waituntilterminated skipifdoesntexist; Verb: "runas"; \
  RunOnceId: "RemoveWaypadDiscoveryRule"

[UninstallDelete]
; The host key, trusted devices and config are deliberately left behind.
; Reinstalling should not make every paired phone pair again, and an
; uninstaller that silently discards keys is one people learn to distrust.
Type: dirifempty; Name: "{app}"

[Code]
// Warn before wiping a paired setup, and only when there is one to wipe.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  StateDir: string;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    StateDir := ExpandConstant('{localappdata}\Waypad\state');
    if DirExists(StateDir) then
    begin
      if MsgBox('Also remove the Waypad host key and the list of paired phones?' + #13#10#13#10 +
                'Keep them if you plan to reinstall: every phone would otherwise have to pair again.',
                mbConfirmation, MB_YESNO) = IDYES then
        DelTree(StateDir, True, True, True);
    end;
  end;
end;
