This docmunet will go over everyting we need to install and build to have a minimal working `rootfs` and `kernel`

# Prerequisites on the Host Machine
we need to install all these libraries 
## First install these from apt
```bash
sudo apt update
sudo apt install -y \
    build-essential \
    git \
    unzip \
    wget \
    libncurses-dev \
    libssl-dev \
    bc \
    cpio \
    rsync \
    python3 \
    file \
    bison \
    flex \
    libelf-dev \
    zlib1g-dev \
    libssl-dev \
    libncurses-dev \
    dwarves \
    pkg-config
```
## Get Buildroot
- [ ] look more at why we use this tool, but for now its simple enough, we just clone the whole repo and extract it
```bash
wget https://buildroot.org/downloads/buildroot-2024.02.tar.gz
tar xf buildroot-2024.02.tar.gz
cd buildroot-2024.02
```
