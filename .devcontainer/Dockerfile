# Dockerfile
FROM ubuntu:latest
RUN apt update -y
RUN apt install git curl sudo -y
RUN curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install linux \
  --extra-conf "sandbox = false" \
  --init none \
  --no-confirm
ENV PATH="${PATH}:/nix/var/nix/profiles/default/bin"


# From HERE

# Create developer user and set up environment
RUN groupadd -g 2000 developer && \
    useradd -m -u 2000 -g sudo -g developer -s /bin/bash developer
RUN  mkdir -p /etc/sudoers.d && \
    echo 'developer ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/developer && \
    chmod 0440 /etc/sudoers.d/developer


#RUN    echo 'developer ALL=(root) NOPASSWD:ALL' > /etc/sudoers.d/developer && \
#    chmod 0440 /etc/sudoers.d/developer

# Set up Nix environment for developer
RUN mkdir -p /nix/var/nix/profiles/per-user/developer && \
    mkdir -p /nix/var/nix/gcroots/per-user/developer && \
    chown -R developer:developer /nix/var/nix/profiles/per-user/developer /nix/var/nix/gcroots/per-user/developer

# Create workspace directory and set permissions
RUN mkdir -p /workspace && \
    chown -R developer:developer /workspace

RUN apt install -y git sudo

# Switch to developer user
USER developer
WORKDIR /workspace

# Set environment variables
ENV USER=developer
ENV HOME=/home/developer
ENV NIX_PATH=/nix/var/nix/profiles/per-user/root/channels
ENV NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
ENV NIXPKGS_ALLOW_UNFREE=1

# Initialize Nix for the user
RUN mkdir -p /home/developer/.config/nix && \
    echo 'experimental-features = nix-command flakes' > /home/developer/.config/nix/nix.conf && \
    chown -R developer:developer /home/developer/.config

# Default command
CMD ["bash", "-l"]
