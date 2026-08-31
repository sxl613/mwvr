#!/usr/bin/bash
set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}ℹ️  $1${NC}"; }
success() { echo -e "${GREEN}✅ $1${NC}"; }
warn() { echo -e "${YELLOW}⚠️  $1${NC}"; }
error() { echo -e "${RED}❌ $1${NC}"; exit 1; }

# Configuration
REMOTE="${REMOTE:-remote}"
PROJECT_DIR="/opt/canis"
IMAGES_TAR="canis-images.tar"
CMD="docker compose -f compose.yml"

NO_CACHE_FLAG=""
USE_LOCAL_IMAGES=false
SKIP_GIT_CHECK=false

# Parse arguments
while [[ $# -gt 0 ]]; do
  case $1 in
    --fresh|-f)
      NO_CACHE_FLAG="--no-cache"
      shift
      ;;
    --local-images|-l)
      USE_LOCAL_IMAGES=true
      shift
      ;;
    --skip-git)
      SKIP_GIT_CHECK=true
      shift
      ;;
    --help|-h)
      echo "Usage: $0 [options]"
      echo "Options:"
      echo "  --fresh, -f        Build the canis image without cache"
      echo "  --local-images, -l Build the image locally and upload a docker save tar"
      echo "  --skip-git         Skip git clean check"
      echo "  --help, -h         Show this help"
      exit 0
      ;;
    *)
      error "Unknown option: $1\nUsage: $0 [--fresh|-f] [--local-images|-l] [--skip-git]"
      ;;
  esac
done

# Cleanup function
cleanup() {
  if [ -f "$IMAGES_TAR" ]; then
    info "Cleaning up local tar file..."
    rm -f "$IMAGES_TAR"
  fi
}
trap cleanup EXIT

# 1. Local Environment Checks
if [ "$SKIP_GIT_CHECK" = false ]; then
  if [[ -n $(git status --porcelain) ]]; then
    warn "Working directory is dirty. You might be deploying uncommitted changes."
    read -p "Continue anyway? (y/N) " confirm
    if [[ $confirm != [yY] ]]; then
      error "Deployment aborted."
    fi
  fi
fi

info "Deploying to $REMOTE..."

# 2. Build locally if requested
if [ "$USE_LOCAL_IMAGES" = true ]; then
  info "Building canis image locally..."
  $CMD build $NO_CACHE_FLAG canis

  info "Exporting canis image to tar..."
  IMAGE_ID=$($CMD images -q canis)
  docker save -o "$IMAGES_TAR" $IMAGE_ID

  info "Uploading image to remote..."
  rsync -ahz --progress "$IMAGES_TAR" "$REMOTE:$PROJECT_DIR/"
fi

# 3. Sync project files
info "Syncing project files..."
# NOTE: trailing slashes on source AND destination are deliberate -
# files must land in /opt/canis directly (that's the live compose dir),
# not in a nested /opt/canis/canis/ the way `rsync src dest` would.
rsync -ahz --delete \
  --exclude='.git/' \
  --exclude='.claude/' \
  --exclude='target/' \
  --exclude='shares.db*' \
  --exclude='thumbs/' \
  --exclude='.env' \
  --exclude='*.tar' \
  ./ "$REMOTE:$PROJECT_DIR/"

# 4. Remote execution
info "Executing remote deployment commands..."

# Construct the remote script
REMOTE_SCRIPT=$(cat <<'DEPLOY_EOF'
  set -euo pipefail
  cd "$PROJECT_DIR"

  # Sanity check: compose requires these in /opt/canis/.env to even parse.
  if [ ! -f .env ]; then
    echo "❌ .env is missing on the remote ($PROJECT_DIR/.env)."
    echo "   Deploying now would fail; copy it over first."
    exit 1
  fi

  # Load image if uploaded
  if [ -f "$IMAGES_TAR" ]; then
    echo "🐳 Loading local image..."
    docker load -i "$IMAGES_TAR"
    rm -f "$IMAGES_TAR"
  fi

  echo "📥 Pulling base images (cloudflared)..."
  $

  if [ "$USE_LOCAL_IMAGES" = false ]; then
    echo "🔨 Building canis image on remote..."
    $CMD build $NO_CACHE_FLAG canis
  fi

  # --force-recreate is required: the image tag is always canis:latest, so
  # compose wouldn't otherwise notice the freshly built image.
  echo "🚀 (Re)creating containers..."
  $CMD up -d --force-recreate --remove-orphans

  echo "⏳ Waiting for canis to be healthy..."
  MAX_RETRIES=24
  COUNT=0
  while [ $COUNT -lt $MAX_RETRIES ]; do
    # First check the container is running (not restarting/crashed)
    CONTAINER_STATUS=$(docker inspect --format='{{.State.Status}}' $($CMD ps -q canis) 2>/dev/null || echo "not-found")
    if [ "$CONTAINER_STATUS" != "running" ]; then
      echo "... container status: $CONTAINER_STATUS - waiting ($((COUNT + 1))/$MAX_RETRIES)"
      sleep 5
      COUNT=$((COUNT + 1))
      continue
    fi

    # Then check PID 1 (the canis binary) is alive inside the container
    if $CMD exec -T canis sh -c 'kill -0 1' >/dev/null 2>&1; then
      echo "✨ canis is healthy!"
      break
    fi

    echo "... app starting ($((COUNT + 1))/$MAX_RETRIES)"
    sleep 5
    COUNT=$((COUNT + 1))
  done

  if [ $COUNT -eq $MAX_RETRIES ]; then
    echo "❌ canis failed to become healthy in time."
    echo "--- last 50 app logs ---"
    $CMD logs --tail=50 canis
    exit 1
  fi

  echo "🧹 Cleaning up old images..."
  docker image prune -f

  echo "📊 Service Status:"
  $CMD ps
DEPLOY_EOF
)

# Pass variables into the remote script
ssh "$REMOTE" "PROJECT_DIR='$PROJECT_DIR' IMAGES_TAR='$IMAGES_TAR' CMD='$CMD' USE_LOCAL_IMAGES='$USE_LOCAL_IMAGES' NO_CACHE_FLAG='$NO_CACHE_FLAG' bash -s" <<DEPLOY_EOF
$REMOTE_SCRIPT
DEPLOY_EOF

success "Deployment complete!"
