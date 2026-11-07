#!/bin/sh
printf '%s\n' "$*" >> "$DOCKER_LOG"
case "$1" in
    image)
        printf 'linux/amd64\n'
        ;;
    tag)
        [ "$3" = hluk-node-rootfs:latest ] || exit 2
        printf '%s' "$2" > "$DOCKER_STATE"
        ;;
    buildx)
        [ "$2" = build ] || exit 3
        shift 2
        context=
        dockerfile=
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --build-context) context="$2"; shift ;;
                --file) dockerfile="$2"; shift ;;
            esac
            shift
        done
        while read -r instruction source stage alias; do
            if [ "$instruction" = FROM ] && [ "$alias" = rootfs ]; then
                case "$source" in
                    *@sha256:*)
                        image="$source"
                        ;;
                    hluk-node-rootfs:latest)
                        case "$context" in
                            "$source"=docker-image://*)
                                image="${context#*=docker-image://}"
                                ;;
                            *)
                                IFS= read -r image < "$DOCKER_STATE" || [ -n "$image" ] || exit 7
                                ;;
                        esac
                        ;;
                    *)
                        exit 4
                        ;;
                esac
                printf '%s\n' "$image"
                exit 0
            fi
        done < "$dockerfile"
        exit 4
        ;;
    *)
        exit 5
        ;;
esac
