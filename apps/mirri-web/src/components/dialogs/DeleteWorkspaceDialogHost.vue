<!-- apps/mirri-web/src/components/dialogs/DeleteWorkspaceDialogHost.vue -->
<!-- Renders the single global three-choice delete dialog driven by
     useDeleteWorkspaceChoice(). Mount once at the app root; callers elsewhere
     just `await confirmWorkspaceDeletion(ws)`. -->
<script setup lang="ts">
import { useDeleteWorkspaceChoice } from '../../composables/useDeleteWorkspaceChoice';
import DeleteWorkspaceDialog from './DeleteWorkspaceDialog.vue';

const { settle } = useDeleteWorkspaceChoice();
</script>

<template>
  <DeleteWorkspaceDialog
    @purge="settle('purge')"
    @remove="settle('remove')"
    @cancel="settle(null)"
  />
</template>
