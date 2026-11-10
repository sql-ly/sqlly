@echo off
setlocal EnableExtensions

if "%~1"=="config" (
  if "%~2"=="get-contexts" if "%~3"=="-o" if "%~4"=="name" (
    echo dev-cluster
    echo prod-cluster
    exit /b 0
  )
)

if "%~1"=="get" (
  if "%~2"=="ns" if "%~3"=="-o" if "%~4"=="name" (
    echo namespace/default
    echo namespace/data
    exit /b 0
  )
  if "%~2"=="pods" (
    echo pod/postgres-0
    echo pod/mysql-0
    exit /b 0
  )
  if "%~2"=="svc" (
    echo service/postgres
    exit /b 0
  )
  if "%~2"=="deploy" (
    echo deployment/postgres
    exit /b 0
  )
)

if "%~1"=="port-forward" (
  set "TARGET="
  set "REMOTE=5432"
  :pf_loop
  if "%~2"=="" goto pf_run
  if "%~2"=="--address" (
    shift
    shift
    goto pf_loop
  )
  echo %~2| findstr /R "^pod/ ^service/ ^deployment/" >nul && (
    set "TARGET=%~2"
    shift
    goto pf_loop
  )
  echo %~2| findstr /R "^:" >nul && (
    set "REMOTE=%~2"
    set "REMOTE=!REMOTE:~1!"
    shift
    goto pf_loop
  )
  shift
  goto pf_loop
  :pf_run
  for /f "tokens=2 delims=/" %%N in ("%TARGET%") do set "NAME=%%N"
  if "%NAME%"=="missing" (
    echo Error from server (NotFound): pods "missing" not found 1>&2
    exit /b 1
  )
  if "%FAKE_KUBECTL_HANG%"=="1" (
    timeout /t 300 /nobreak >nul
    exit /b 0
  )
  if not defined FAKE_KUBECTL_PORT set FAKE_KUBECTL_PORT=45999
  echo Forwarding from 127.0.0.1:%FAKE_KUBECTL_PORT% -^> %REMOTE%
  :pf_sleep
  timeout /t 1 /nobreak >nul
  goto pf_sleep
)

echo fake-kubectl: unhandled: %* 1>&2
exit /b 1
