// Whether the page is a person's console, read on either side of the render. Its own module for
// the reason appearanceRequest.ts gives: Storybook has no server entry to import.
import { createIsomorphicFn } from "@tanstack/react-start";
import { getRequestHeader } from "@tanstack/react-start/server";
import { isPersonCookie, personBrowser } from "./person";

export const currentPersonMode = createIsomorphicFn()
	.server(() => isPersonCookie(getRequestHeader("cookie")))
	.client(personBrowser);
