import {
    Box,
    Button,
    Center,
    Divider,
    Flex,
    HStack,
    SimpleGrid,
    StackDivider,
    Text,
    useToast,
    VStack,
} from "@chakra-ui/react";
import axios from "axios";
import React, { useState } from "react";
import { useAsync } from "react-async";
import { Route, Routes } from "react-router-dom";
import { useEventSource } from "react-sse-hooks";
import "./App.css";
import CfdOffer from "./components/CfdOffer";
import CfdTile from "./components/CfdTile";
import CurrencyInputField from "./components/CurrencyInputField";
import useLatestEvent from "./components/Hooks";
import NavLink from "./components/NavLink";
import { Cfd, Offer } from "./components/Types";

/* TODO: Change from localhost:8001 */
const BASE_URL = "http://localhost:8001";

interface CfdSellOfferPayload {
    price: number;
    min_quantity: number;
    max_quantity: number;
}

async function postCfdSellOfferRequest(payload: CfdSellOfferPayload) {
    let res = await axios.post(BASE_URL + `/offer/sell`, JSON.stringify(payload));

    if (!res.status.toString().startsWith("2")) {
        console.log("Status: " + res.status + ", " + res.statusText);
        throw new Error("failed to publish new offer");
    }
}

export default function App() {
    let source = useEventSource({ source: BASE_URL + "/maker-feed" });

    const cfds = useLatestEvent<Cfd[]>(source, "cfds");
    const offer = useLatestEvent<Offer>(source, "offer");

    console.log(cfds);

    const balance = useLatestEvent<number>(source, "balance");

    const toast = useToast();
    let [minQuantity, setMinQuantity] = useState<string>("100");
    let [maxQuantity, setMaxQuantity] = useState<string>("1000");
    let [offerPrice, setOfferPrice] = useState<string>("10000");

    const format = (val: any) => `$` + val;
    const parse = (val: any) => val.replace(/^\$/, "");

    let { run: makeNewCfdSellOffer, isLoading: isCreatingNewCfdOffer } = useAsync({
        deferFn: async ([payload]: any[]) => {
            try {
                await postCfdSellOfferRequest(payload as CfdSellOfferPayload);
            } catch (e) {
                const description = typeof e === "string" ? e : JSON.stringify(e);

                toast({
                    title: "Error",
                    description,
                    status: "error",
                    duration: 9000,
                    isClosable: true,
                });
            }
        },
    });

    return (
        <Center marginTop={50}>
            <HStack>
                <Box marginRight={5}>
                    <VStack align={"top"}>
                        <NavLink text={"trade"} path={"trade"} />
                        <NavLink text={"wallet"} path={"wallet"} />
                        <NavLink text={"settings"} path={"settings"} />
                    </VStack>
                </Box>
                <Box width={1200} height="100%">
                    <Routes>
                        <Route
                            path="trade"
                            element={<Flex direction={"row"} height={"100%"}>
                                <Flex direction={"row"} width={"100%"}>
                                    <VStack
                                        spacing={5}
                                        shadow={"md"}
                                        padding={5}
                                        width={"100%"}
                                        divider={<StackDivider borderColor="gray.200" />}
                                    >
                                        <Box width={"100%"} overflow={"scroll"}>
                                            <SimpleGrid columns={2} spacing={10}>
                                                {cfds && cfds.map((cfd, index) =>
                                                    <CfdTile
                                                        key={"cfd_" + index}
                                                        index={index}
                                                        cfd={cfd}
                                                    />
                                                )}
                                            </SimpleGrid>
                                        </Box>
                                    </VStack>
                                </Flex>
                                <Flex width={"50%"} marginLeft={5}>
                                    <VStack spacing={5} shadow={"md"} padding={5} align={"stretch"}>
                                        <HStack>
                                            <Text align={"left"}>Your balance:</Text>
                                            <Text>{balance}</Text>
                                        </HStack>
                                        <HStack>
                                            <Text align={"left"}>Current Price:</Text>
                                            <Text>{49000}</Text>
                                        </HStack>
                                        <HStack>
                                            <Text>Min Quantity:</Text>
                                            <CurrencyInputField
                                                onChange={(valueString: string) => setMinQuantity(parse(valueString))}
                                                value={format(minQuantity)}
                                            />
                                        </HStack>
                                        <HStack>
                                            <Text>Min Quantity:</Text>
                                            <CurrencyInputField
                                                onChange={(valueString: string) => setMaxQuantity(parse(valueString))}
                                                value={format(maxQuantity)}
                                            />
                                        </HStack>
                                        <HStack>
                                            <Text>Offer Price:</Text>
                                        </HStack>
                                        <CurrencyInputField
                                            onChange={(valueString: string) => setOfferPrice(parse(valueString))}
                                            value={format(offerPrice)}
                                        />
                                        <Text>Leverage:</Text>
                                        <Flex justifyContent={"space-between"}>
                                            <Button disabled={true}>x1</Button>
                                            <Button disabled={true}>x2</Button>
                                            <Button colorScheme="blue" variant="solid">x{5}</Button>
                                        </Flex>
                                        <VStack>
                                            <Center><Text>Maker UI</Text></Center>
                                            <Button
                                                disabled={isCreatingNewCfdOffer}
                                                variant={"solid"}
                                                colorScheme={"blue"}
                                                onClick={() => {
                                                    let payload: CfdSellOfferPayload = {
                                                        price: Number.parseFloat(offerPrice),
                                                        min_quantity: Number.parseFloat(minQuantity),
                                                        max_quantity: Number.parseFloat(maxQuantity),
                                                    };
                                                    makeNewCfdSellOffer(payload);
                                                }}
                                            >
                                                {offer ? "Update Sell Offer" : "Create Sell Offer"}
                                            </Button>
                                            <Divider />
                                            <Box width={"100%"} overflow={"scroll"}>
                                                <Box>
                                                    {offer
                                                        && <CfdOffer
                                                            offer={offer}
                                                        />}
                                                </Box>
                                            </Box>
                                        </VStack>
                                    </VStack>
                                </Flex>
                            </Flex>}
                        >
                        </Route>
                        <Route
                            path="wallet"
                            element={<Center height={"100%"} shadow={"md"}>
                                <Box>
                                    <Text>Wallet</Text>
                                </Box>
                            </Center>}
                        >
                        </Route>
                        <Route
                            path="settings"
                            element={<Center height={"100%"} shadow={"md"}>
                                <Box>
                                    <Text>Settings</Text>
                                </Box>
                            </Center>}
                        >
                        </Route>
                    </Routes>
                </Box>
            </HStack>
        </Center>
    );
}