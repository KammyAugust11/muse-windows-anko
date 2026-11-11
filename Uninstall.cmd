@echo off
setlocal
set "DEST=%LOCALAPPDATA%\Programs\Muse"
rem The app normally sits in the tray; while it runs its files cannot be removed.
taskkill /im anko.exe /f >nul 2>&1
powershell -NoProfile -ExecutionPolicy Bypass -Command "Remove-Item -Force (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Muse.lnk') -ErrorAction SilentlyContinue; Remove-Item -Recurse -Force (Join-Path ([Environment]::GetFolderPath('Programs')) 'Muse') -ErrorAction SilentlyContinue; Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'Muse' -ErrorAction SilentlyContinue"
if exist "%DEST%" rmdir /s /q "%DEST%"
if exist "%DEST%" (
  echo Could not remove "%DEST%" - close Muse and run this again.
  pause
  exit /b 1
)
echo Uninstalled the app (autostart removed too). Login data in %%APPDATA%%\Muse was kept.
echo Delete that folder manually if you want a full cleanup.
pause
