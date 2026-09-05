@echo off
rem Пересборка и обновление бинаря, на который смотрит ярлык.
cd /d "%~dp0"
cargo build --release || exit /b 1
taskkill /F /IM ghost.exe >nul 2>&1
rem Windows отпускает файл не мгновенно: без паузы copy падает с отказом в доступе.
ping -n 2 127.0.0.1 >nul
copy /y C:\gt\release\ghost.exe ghost.exe >nul || (echo не удалось обновить ghost.exe & exit /b 1)
echo ghost.exe обновлён
