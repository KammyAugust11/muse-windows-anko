@echo off
setlocal
rem Installs anko.exe to %LOCALAPPDATA%\Programs\Muse and creates shortcuts.
rem Also registers logon autostart (starts hidden in the task tray);
rem you can turn that off later from the tray icon's menu.
set "SRC=%~dp0anko.exe"
set "DEST=%LOCALAPPDATA%\Programs\Muse"
if not exist "%SRC%" (
  echo anko.exe not found next to this script.
  echo Build it first ^(cargo build --release^) and copy target\release\anko.exe here,
  echo or use the ZIP from the Releases page.
  pause
  exit /b 1
)
rem A running copy keeps anko.exe locked, so stop it before replacing the file.
taskkill /im anko.exe /f >nul 2>&1
if not exist "%DEST%" mkdir "%DEST%"
copy /y "%SRC%" "%DEST%\anko.exe" >nul
if errorlevel 1 (
  echo Could not copy anko.exe to "%DEST%".
  pause
  exit /b 1
)
rem Optional companions: the loader DLL only exists for GNU-toolchain builds
rem (MSVC builds link it statically), and icon.ico only ships in the ZIP.
if exist "%~dp0WebView2Loader.dll" copy /y "%~dp0WebView2Loader.dll" "%DEST%\WebView2Loader.dll" >nul
if exist "%~dp0icon.ico" copy /y "%~dp0icon.ico" "%DEST%\icon.ico" >nul
rem Paths are read from the environment inside PowerShell rather than pasted
rem into the command text, so quotes or odd characters in a user name are safe.
powershell -NoProfile -ExecutionPolicy Bypass -Command "$d=$env:DEST; $exe=Join-Path $d 'anko.exe'; $ico=Join-Path $d 'icon.ico'; if(-not (Test-Path -LiteralPath $ico)){$ico=$exe}; $w=New-Object -ComObject WScript.Shell; $p=Join-Path ([Environment]::GetFolderPath('Programs')) 'Muse'; New-Item -ItemType Directory -Force $p | Out-Null; foreach($lnk in @((Join-Path ([Environment]::GetFolderPath('Desktop')) 'Muse.lnk'), (Join-Path $p 'Muse.lnk'))){ $s=$w.CreateShortcut($lnk); $s.TargetPath=$exe; $s.WorkingDirectory=$d; $s.IconLocation=$ico; $s.Save() }; $q=[string][char]34; New-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Muse' -Value ($q+$exe+$q+' --tray') -PropertyType String -Force | Out-Null"
if errorlevel 1 (
  echo The app was copied, but creating shortcuts / autostart failed.
  pause
  exit /b 1
)
echo.
echo Installed. Look for "Muse" (Anko icon) on your Desktop / Start Menu.
echo Anko will also start (hidden, in the task tray) when you sign in to Windows.
echo Login data is stored in %%APPDATA%%\Muse (separate from your browser).
pause
