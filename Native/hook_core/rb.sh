SPATH=$(dirname $0)
. $SPATH/shutil.sh
REQ=x86_64
check_tc $REQ

if [ "$1" == "profile" ]; then
    BCMD="cargo build --release --features=profile"
else
    BCMD="cargo build --release"
fi

# NOTE: use git bash, not WSL
GPATH="/f/Guild Wars 2"
if [ ! -d "$GPATH" ]; then
    GPATH="/c/Guild Wars 2"
fi
if [ ! -d "$GPATH" ]; then
    echo "Can't find game"
    exit 1
fi

# select d3d 9 or 11 via first argument, file goes in a different place in each case
if [ "$1" == "11" ]; then
    DEST=d3d11.dll
else
    DEST=bin64/d3d9.dll
fi

$BCMD && cp -v target/release/hook_core.dll "$GPATH/$DEST" && RUST_BACKTRACE=1 "$GPATH/Gw2-64.exe"
