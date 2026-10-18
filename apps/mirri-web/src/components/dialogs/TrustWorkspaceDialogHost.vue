<!-- apps/mirri-web/src/components/dialogs/TrustWorkspaceDialogHost.vue -->
<!-- Renders the single global workspace trust confirmation driven by
     useWorkspaceTrustPrompt(). Mount once at the app root; callers elsewhere
     just `await confirmWorkspaceTrust(ws)`. -->
<script setup lang="ts">
import { useWorkspaceTrustPrompt } from '../../composables/useWorkspaceTrustPrompt';
import TrustWorkspaceDialog from './TrustWorkspaceDialog.vue';

const { settle } = useWorkspaceTrustPrompt();
</script>

<template>
  <TrustWorkspaceDialog
    @trust="settle(true)"
    @later="settle(false)"
  />
</template>
