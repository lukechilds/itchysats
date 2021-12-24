import { ToastId, useToast } from "@chakra-ui/react";
import { useEffect, useRef, useState } from "react";

export function useEventSource(url: string, withCredentials?: boolean) {
    const [source, setSource] = useState<EventSource | null>(null);
    const [isConnected, setIsConnected] = useState<boolean>(true);

    // Construct a new event source if the arguments to this hook change
    useEffect(() => {
        const es = new EventSource(url, { withCredentials });
        setSource(es);

        es.addEventListener("error", () => {
            setIsConnected(false);
            setSource(null);
        });

        return () => {
            setSource(null);
            es.close();
        };
    }, [url, withCredentials]);

    const timeoutHandle = useRef<NodeJS.Timeout | null>(null);

    // Initial timeout which will declare the event source
    // disconnected if we don't receive a heartbeat in time
    useEffect(() => {
        const timeout = setTimeout(() => {
            setIsConnected(false);
            setSource(null);
        }, HEARTBEAT_TIMEOUT);
        timeoutHandle.current = timeout;
        return;
    }, []);

    // If a heartbeat is not received within HEARTBEAT_TIMEOUT
    // milliseconds, declare the event source disconnected
    useEffect(() => {
        const heartbeatCallback = () => {
            if (timeoutHandle.current) clearTimeout(timeoutHandle.current);
            const timeout = setTimeout(() => {
                setIsConnected(false);
                setSource(null);
            }, HEARTBEAT_TIMEOUT);
            timeoutHandle.current = timeout;
        };

        if (source && source.readyState !== 2) {
            source.addEventListener(HEARTBEAT_EVENT_NAME, heartbeatCallback);
            return () => source.removeEventListener(HEARTBEAT_EVENT_NAME, heartbeatCallback);
        }
        return undefined;
    }, [source]);

    const toast = useToast();
    const toastId = useRef<ToastId | undefined>(undefined);
    if (!isConnected && !toastId.current) {
        toastId.current = toast(
            {
                title: "Connection error",
                description: "Please ensure taker daemon is up and refresh the page to reconnect.",
                status: "error",
                position: "top",
                duration: null,
                isClosable: false,
            },
        );
    }

    return source;
}

const HEARTBEAT_EVENT_NAME = "heartbeat";
const HEARTBEAT_TIMEOUT = 10000; // milliseconds

export type EventSourceEvent = Event & { data: string };
